//! HTTP handlers: an unmodified `wasi:http` component
//! (`wasi:http/proxy`, exporting `wasi:http/incoming-handler`) gets a new
//! instance for each request on its routes, with streaming bodies.
//!
//! The instance is like a connection handler's (`crate::listener`): a pid,
//! the app's limits and host services, no mailbox, and nothing restarts it.
//! It takes a Store slot for the request's life; none free answers `503`
//! with `Retry-After`.
//!
//! - **Time:** the route's timeout runs until the response's headers, then
//!   again as an idle timer while either body moves, so a long download is
//!   not cut while it flows. Guest execution is bounded by
//!   `callback_deadline_ms`, as for one callback.
//! - **Failures:** a trap, a timeout or a handler that never sets a
//!   response before its headers answers `500` (`504` for the timeout);
//!   after the headers, the body stream is cut (the connection closes
//!   without the last chunk). A handler that drops its body without
//!   finishing it and returns normally ends the body cleanly: wasmtime 49
//!   and 50 do not tell the two apart.
//! - **Limits:** the route's `max_body` bounds the request body as it is
//!   read; the guest sees a body-size error past it.
//! - **Upgrades** apply to new requests; requests in flight finish on the
//!   version they started with.
//!
//! Without the `egress` cargo feature `wasi:http` is not linked, and
//! handlers cannot be loaded.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::spec::InstanceSpec;

/// A compiled HTTP-handler component, linked against the host.
#[derive(Clone)]
pub struct LoadedHandler {
    #[cfg(feature = "egress")]
    pub(crate) pre: wasmtime_wasi_http::p2::bindings::ProxyPre<crate::state::InstanceState>,
    pub(crate) stubbed: Arc<Vec<String>>,
}

impl LoadedHandler {
    /// Imported interfaces no host here provides, linked to deny stubs.
    #[must_use]
    pub fn stubbed_imports(&self) -> &[String] {
        &self.stubbed
    }
}

impl std::fmt::Debug for LoadedHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedHandler")
            .field("stubbed", &self.stubbed)
            .finish_non_exhaustive()
    }
}

/// An app's handler as the ingress runs it: the component, the spec its
/// instances run with, and its requests in flight (kept across upgrades).
#[derive(Debug)]
// Without `egress` no handler is ever loaded, so nothing reads these.
#[cfg_attr(not(feature = "egress"), allow(dead_code))]
pub(super) struct Handler {
    pub(super) loaded: LoadedHandler,
    pub(super) spec: InstanceSpec,
    pub(super) active: Arc<AtomicUsize>,
}

impl Handler {
    pub(super) fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }
}

#[cfg(feature = "egress")]
pub(super) use serving::serve;

#[cfg(feature = "egress")]
mod serving {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use http_body_util::{BodyExt, Limited};
    use hyper::body::{Body, Frame, Incoming, SizeHint};
    use hyper::header::{HeaderValue, RETRY_AFTER};
    use hyper::{Request, StatusCode};
    use wasmtime_wasi_http::p2::bindings::http::types::Scheme;

    use super::Handler;
    use crate::delivery;
    use crate::http::{BoxError, HttpRoute, Reply, status};
    use crate::instance::{self, classify, new_store};
    use crate::listener::Activity;
    use crate::message::ExitReason;
    use crate::pid::Pid;
    use crate::probe;
    use crate::runtime::RuntimeInner;
    use crate::services::egress;
    use crate::state::InstanceState;

    /// Serve one request with a new instance of `handler`. `failed` counts
    /// a handler that trapped, timed out or never answered.
    pub(in crate::http) async fn serve(
        rt: &Arc<RuntimeInner>,
        handler: &Arc<Handler>,
        route: &HttpRoute,
        scheme: &'static str,
        req: Request<Incoming>,
        failed: Arc<dyn Fn() + Send + Sync>,
    ) -> Reply {
        rt.pool_memory.taken();
        let Ok(slot) = rt.slots.clone().try_acquire_owned() else {
            let mut r = status(StatusCode::SERVICE_UNAVAILABLE, "overloaded");
            r.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("1"));
            return r;
        };
        let pid = Pid {
            node: rt.node(),
            id: rt.next_id(),
            incarnation: 1,
        };
        let spec = &handler.spec;
        let (_handle, ctx) = instance::detached(pid, spec, rt.clone());
        let state = match InstanceState::new(pid, spec, ctx) {
            Ok(state) => state,
            Err(why) => {
                finish(rt, pid, &ExitReason::Crashed(why), &*failed);
                return status(StatusCode::INTERNAL_SERVER_ERROR, "the handler failed");
            }
        };
        let mut store = new_store(&rt.engine, state);
        let activity = Arc::new(Activity::new());
        let max_body = route.max_body;
        let req = req.map(|body| Tracked {
            inner: Limited::new(body, usize::try_from(max_body).unwrap_or(usize::MAX))
                .map_err(move |e| body_error(&e, max_body)),
            activity: activity.clone(),
        });
        let scheme = if scheme == "https" {
            Scheme::Https
        } else {
            Scheme::Http
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let resources = {
            let mut view = egress::incoming_view(store.data_mut());
            view.new_incoming_request(scheme, req)
                .and_then(|req| Ok((req, view.new_response_outparam(tx)?)))
        };
        let (req, out) = match resources {
            Ok(r) => r,
            Err(e) => {
                let reason = ExitReason::Crashed(e.to_string());
                finish(rt, pid, &reason, &*failed);
                return status(StatusCode::BAD_REQUEST, "unreadable request");
            }
        };
        store.data_mut().clock.start(spec.callback_deadline);
        let pre = handler.loaded.pre.clone();
        let timeout = route.timeout;
        // Set before the Store is dropped when the instance failed: wasmtime
        // ends a body whose Store went away as if it had finished.
        let cut = Arc::new(AtomicBool::new(false));
        let active = handler.active.clone();
        active.fetch_add(1, Ordering::Relaxed);
        let task = {
            let (rt, activity, failed, cut) =
                (rt.clone(), activity.clone(), failed.clone(), cut.clone());
            tokio::spawn(async move {
                let run = async {
                    let result = async {
                        let proxy = pre.instantiate_async(&mut store).await?;
                        proxy
                            .wasi_http_incoming_handler()
                            .call_handle(&mut store, req, out)
                            .await
                    }
                    .await;
                    match result {
                        Ok(()) => ExitReason::Normal,
                        Err(err) => classify(&err, store.data()),
                    }
                };
                let reason = tokio::select! {
                    reason = run => reason,
                    () = expire(&activity, timeout) => ExitReason::Timeout,
                };
                cut.store(!reason.is_normal(), Ordering::Release);
                drop(store);
                drop(slot);
                active.fetch_sub(1, Ordering::Relaxed);
                finish(&rt, pid, &reason, &*failed);
                reason
            })
        };
        match rx.await {
            Ok(Ok(response)) => {
                activity.respond();
                response.map(|body| {
                    Cut {
                        inner: Tracked {
                            inner: body.map_err(|e| Box::new(e) as BoxError),
                            activity,
                        },
                        cut,
                    }
                    .boxed_unsync()
                })
            }
            Ok(Err(code)) => {
                tracing::debug!(%pid, ?code, "HTTP handler answered an error");
                status(StatusCode::INTERNAL_SERVER_ERROR, "the handler failed")
            }
            // The handler ended without setting a response.
            Err(_) => match task.await {
                Ok(ExitReason::Timeout) => {
                    status(StatusCode::GATEWAY_TIMEOUT, "no response in time")
                }
                Ok(ExitReason::Normal) => {
                    failed();
                    status(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "the handler set no response",
                    )
                }
                _ => status(StatusCode::INTERNAL_SERVER_ERROR, "the handler failed"),
            },
        }
    }

    /// Resolve when the request has run out of time: no response headers
    /// within `limit`, then nothing moving in either body for `limit`.
    async fn expire(activity: &Activity, limit: std::time::Duration) {
        tokio::time::sleep(limit).await;
        if activity.responded() {
            activity.idle_for(limit).await;
        }
    }

    /// What a request body error becomes for the guest.
    fn body_error(e: &BoxError, max_body: u64) -> wasmtime_wasi_http::Error {
        use wasmtime_wasi_http::Error;
        if e.downcast_ref::<http_body_util::LengthLimitError>()
            .is_some()
        {
            Error::HttpRequestBodySize(Some(max_body))
        } else {
            Error::InternalError(Some(e.to_string()))
        }
    }

    fn finish(rt: &RuntimeInner, pid: Pid, reason: &ExitReason, failed: &(dyn Fn() + Send + Sync)) {
        if !reason.is_normal() {
            failed();
            tracing::warn!(%pid, %reason, "HTTP handler failed");
        }
        rt.registry.unregister_all(pid);
        probe::instance_exit(pid, reason);
        delivery::notify_exit(rt, pid, reason);
    }

    /// A response body that ends in an error, not cleanly, when the instance
    /// writing it failed: the client sees a cut stream (no last chunk).
    struct Cut<B> {
        inner: B,
        cut: Arc<AtomicBool>,
    }

    impl<B: Body<Data = Bytes, Error = BoxError> + Unpin> Body for Cut<B> {
        type Data = Bytes;
        type Error = BoxError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Ready(None) if self.cut.load(Ordering::Acquire) => {
                    Poll::Ready(Some(Err("the HTTP handler failed".into())))
                }
                poll => poll,
            }
        }

        fn is_end_stream(&self) -> bool {
            !self.cut.load(Ordering::Acquire) && self.inner.is_end_stream()
        }

        fn size_hint(&self) -> SizeHint {
            self.inner.size_hint()
        }
    }

    /// A body that records activity on each frame.
    struct Tracked<B> {
        inner: B,
        activity: Arc<Activity>,
    }

    impl<B: Body<Data = Bytes> + Unpin> Body for Tracked<B> {
        type Data = Bytes;
        type Error = B::Error;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
            let poll = Pin::new(&mut self.inner).poll_frame(cx);
            if matches!(poll, Poll::Ready(Some(Ok(_)))) {
                self.activity.touch();
            }
            poll
        }

        fn is_end_stream(&self) -> bool {
            self.inner.is_end_stream()
        }

        fn size_hint(&self) -> SizeHint {
            self.inner.size_hint()
        }
    }
}
