//! `wasi:http/outgoing-handler`: HTTP requests from apps (ROADMAP E2).
//!
//! wasmtime-wasi-http implements the interface; its send hook is this
//! module's filter. A request goes out only if the operator granted the
//! capability, its scheme, host and port match an entry of the manifest's
//! `allow` (the `policy` module), and its method is in `methods` (when listed). It
//! then goes through the node's connection pool (the `client` module), at most
//! `max_concurrent` per app at once (node config's default otherwise). A
//! host name that resolves only to loopback or link-local addresses is
//! refused unless node config sets `allow_loopback`; an address written in
//! an allow entry is reached as written. Refusals are wasi:http error codes
//! (`HTTP-request-denied`, `connection-limit-reached`,
//! `destination-IP-prohibited`), never traps.
//!
//! Node config's `[egress] allow` is a ceiling every app's `allow` must fit
//! inside; the node checks it at deploy ([`within`]).
//!
//! Without the `egress` cargo feature the interface is not linked, so an
//! app importing it gets the usual deny stubs.

mod policy;

#[cfg(feature = "egress")]
mod client;

pub use policy::within;

use std::sync::Arc;

use paddock_bundle::manifest::{AppSpec, Capability};
use wasmtime::component::Linker;

use super::{HostState, InstanceInfo, Service};
use crate::config::RuntimeConfig;
use crate::error::HostError;

/// The egress service.
pub(crate) struct Egress;

/// What the manifest asks of egress for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct EgressSpec {
    /// Allow entries (`https://api.example.com`, `*.internal`).
    pub allow: Vec<String>,
    /// Methods allowed, upper case; empty for any.
    pub methods: Vec<String>,
    /// Requests in flight at once; `None` for node config's default.
    pub max_concurrent: Option<u32>,
}

/// Node-wide egress state.
pub(crate) struct Shared {
    #[cfg(feature = "egress")]
    pool: client::Pool,
    #[cfg(feature = "egress")]
    config: crate::config::Egress,
    /// Each app's limit on requests in flight, and its semaphore.
    #[cfg(feature = "egress")]
    limits:
        std::sync::Mutex<std::collections::HashMap<String, (usize, Arc<tokio::sync::Semaphore>)>>,
}

/// Egress state for one instance.
pub(crate) struct Instance {
    #[cfg(feature = "egress")]
    ctx: wasmtime_wasi_http::WasiHttpCtx,
    #[cfg(feature = "egress")]
    hooks: hooks::Hooks,
}

impl Service for Egress {
    const CAPABILITY: Capability = Capability::HttpOutgoing;
    #[cfg(feature = "egress")]
    const INTERFACES: &'static [&'static str] = &["wasi:http/outgoing-handler", "wasi:http/types"];
    #[cfg(not(feature = "egress"))]
    const INTERFACES: &'static [&'static str] = &[];
    type Spec = EgressSpec;
    type Shared = Shared;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> EgressSpec {
        manifest
            .capabilities
            .http_outgoing
            .as_ref()
            .map(|s| EgressSpec {
                allow: s.allow.clone(),
                methods: s.methods.clone(),
                max_concurrent: s.max_concurrent,
            })
            .unwrap_or_default()
    }

    fn shared(config: &RuntimeConfig) -> Result<Shared, HostError> {
        #[cfg(not(feature = "egress"))]
        let _ = config;
        Ok(Shared {
            #[cfg(feature = "egress")]
            pool: client::Pool::new(&config.egress).map_err(HostError::Egress)?,
            #[cfg(feature = "egress")]
            config: config.egress.clone(),
            #[cfg(feature = "egress")]
            limits: std::sync::Mutex::default(),
        })
    }

    #[cfg_attr(not(feature = "egress"), allow(unused_variables))]
    fn instance(shared: &Arc<Shared>, spec: &EgressSpec, info: &InstanceInfo<'_>) -> Instance {
        Instance {
            #[cfg(feature = "egress")]
            ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            #[cfg(feature = "egress")]
            hooks: hooks::Hooks::new(shared, spec, info),
        }
    }

    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> wasmtime::Result<()> {
        #[cfg(feature = "egress")]
        {
            use wasmtime_wasi_http::WasiHttp;
            use wasmtime_wasi_http::p2::bindings::LinkOptions;
            use wasmtime_wasi_http::p2::bindings::http::{outgoing_handler, types};

            outgoing_handler::add_to_linker::<T, WasiHttp>(linker, view::<T>)?;
            types::add_to_linker::<T, WasiHttp>(linker, &LinkOptions::default().into(), view::<T>)?;
        }
        #[cfg(not(feature = "egress"))]
        let _ = linker;
        Ok(())
    }
}

/// wasi:http's view of an instance: the egress state and the instance's
/// resource table, which `wasi:io` streams share.
#[cfg(feature = "egress")]
fn view<T: HostState<Egress>>(t: &mut T) -> wasmtime_wasi_http::WasiHttpCtxView<'_> {
    let super::ServiceCtx { state, table, .. } = t.service();
    wasmtime_wasi_http::WasiHttpCtxView {
        ctx: &mut state.ctx,
        table,
        hooks: &mut state.hooks,
    }
}

/// wasi:http's view of an instance, for the HTTP ingress to hand an HTTP
/// handler its request (`crate::http`): the same state the guest's
/// `wasi:http/types` calls reach.
#[cfg(feature = "egress")]
pub(crate) fn incoming_view(
    state: &mut crate::state::InstanceState,
) -> wasmtime_wasi_http::WasiHttpCtxView<'_> {
    view(state)
}

#[cfg(feature = "egress")]
mod hooks {
    use std::future::Future;
    use std::sync::{Arc, PoisonError};

    use tokio::sync::Semaphore;
    use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

    use super::policy::{self, Allow, Scheme};
    use super::{EgressSpec, InstanceInfo, Shared};
    use crate::metrics::AppStats;

    const SENT: usize = 0;
    const DENIED: usize = 1;
    const LIMITED: usize = 2;
    const FAILED: usize = 3;

    type Sent = Box<
        dyn Future<
                Output = Result<
                    (
                        hyper::Response<WasiBody>,
                        Box<dyn Future<Output = Result<(), Error>> + Send>,
                    ),
                    Error,
                >,
            > + Send,
    >;

    /// The filter for one instance.
    pub(crate) struct Hooks {
        granted: bool,
        app: String,
        allow: Vec<Allow>,
        methods: Vec<String>,
        limit: Arc<Semaphore>,
        shared: Arc<Shared>,
        stats: Arc<AppStats>,
    }

    impl Hooks {
        pub(crate) fn new(
            shared: &Arc<Shared>,
            spec: &EgressSpec,
            info: &InstanceInfo<'_>,
        ) -> Self {
            let app = info.handle.app().to_string();
            // The node refused bad entries at deploy; keep the good ones.
            let allow = spec
                .allow
                .iter()
                .filter_map(|e| policy::parse(e).ok())
                .collect();
            let max = usize::try_from(spec.max_concurrent.unwrap_or(shared.config.max_concurrent))
                .unwrap_or(usize::MAX)
                .min(Semaphore::MAX_PERMITS);
            let limit = {
                let mut limits = shared.limits.lock().unwrap_or_else(PoisonError::into_inner);
                let entry = limits
                    .entry(app.clone())
                    .or_insert_with(|| (max, Arc::new(Semaphore::new(max))));
                // An upgrade that changes the limit starts a new count;
                // requests in flight finish on the old one.
                if entry.0 != max {
                    *entry = (max, Arc::new(Semaphore::new(max)));
                }
                entry.1.clone()
            };
            Self {
                granted: info.granted,
                stats: info.metrics.stats(&app),
                app,
                allow,
                methods: spec.methods.clone(),
                limit,
                shared: shared.clone(),
            }
        }

        /// Why `request` may not go out, if it may not.
        fn refuse(&self, request: &hyper::Request<WasiBody>) -> Option<&'static str> {
            if !self.granted {
                return Some("the capability is not granted");
            }
            let uri = request.uri();
            let scheme = match uri.scheme_str() {
                Some("https") => Scheme::Https,
                Some("http") => Scheme::Http,
                _ => return Some("not http or https"),
            };
            let Some(host) = uri.host() else {
                return Some("no host");
            };
            let port = uri.port_u16().unwrap_or(match scheme {
                Scheme::Https => 443,
                Scheme::Http => 80,
            });
            if !self.allow.iter().any(|a| a.allows(scheme, host, port)) {
                return Some("not in the manifest's allow list");
            }
            if !self.methods.is_empty()
                && !self.methods.iter().any(|m| m == request.method().as_str())
            {
                return Some("method not in the manifest's methods");
            }
            None
        }
    }

    fn fail(error: Error) -> Sent {
        Box::new(async move { Err(error) })
    }

    impl WasiHttpHooks for Hooks {
        fn send_request(
            &mut self,
            request: hyper::Request<WasiBody>,
            options: Option<RequestOptions>,
            _fut: Box<dyn Future<Output = Result<(), Error>> + Send>,
        ) -> Sent {
            if let Some(why) = self.refuse(&request) {
                tracing::debug!(app = %self.app, uri = %request.uri(), why, "egress request denied");
                self.stats.egress(DENIED);
                return fail(Error::HttpRequestDenied);
            }
            let Ok(permit) = self.limit.clone().try_acquire_owned() else {
                self.stats.egress(LIMITED);
                return fail(Error::ConnectionLimitReached);
            };
            let config = &self.shared.config;
            let shorter =
                |guest: Option<std::time::Duration>, node| guest.map_or(node, |g| g.min(node));
            let first_byte = shorter(
                options.and_then(|o| o.first_byte_timeout),
                config.first_byte_timeout,
            );
            let between = shorter(
                options.and_then(|o| o.between_bytes_timeout),
                config.between_bytes_timeout,
            );
            let sent = self.shared.pool.send(request, first_byte, between, permit);
            let stats = self.stats.clone();
            Box::new(async move {
                let response = sent.await;
                stats.egress(if response.is_ok() { SENT } else { FAILED });
                let io: Box<dyn Future<Output = Result<(), Error>> + Send> =
                    Box::new(async { Ok(()) });
                Ok((response?, io))
            })
        }
    }
}
