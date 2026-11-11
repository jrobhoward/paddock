//! `bulkhead:http-egress`: HTTP requests from apps.
//!
//! An app sends a request as a frame, and this module's filter (`Hooks`)
//! checks it. A request goes out only if the operator granted the
//! capability, its scheme, host and port match an entry of the manifest's
//! `allow` (the `policy` module), and its method is in `methods` (when listed). It
//! then goes through the node's connection pool (the `client` module), at most
//! `max_concurrent` per app at once (node config's default otherwise). A
//! host name that resolves only to loopback or link-local addresses is
//! refused unless node config sets `allow_loopback`; an address written in
//! an allow entry is reached as written. Refusals are answers
//! (`HttpError::Denied`, `Limited`, `Prohibited`), never a crash of the
//! instance.
//!
//! Node config's `[egress] allow` is a ceiling every app's `allow` must fit
//! inside; the node checks it at deploy ([`within`]).
//!
//! A native app asks over the protocol (`Instance::native`, ROADMAP Z8f):
//! the same filter, cap, timeouts and pool, with the forbidden headers
//! refused, and the whole response, up to a frame's size, in the answer.
//!
//! Without the `egress` cargo feature the interface is not linked, so an
//! app importing it gets the usual deny stubs, and a native app's request
//! is answered `Other`.

mod policy;

#[cfg(feature = "egress")]
mod client;

pub use policy::within;

use std::sync::Arc;

use bulkhead_bundle::manifest::{AppSpec, Capability};

use super::{InstanceInfo, Service};
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
    hooks: hooks::Hooks,
}

impl Service for Egress {
    const CAPABILITY: Capability = Capability::HttpOutgoing;
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
            hooks: hooks::Hooks::new(shared, spec, info),
        }
    }
}

impl Instance {
    /// An app's HTTP request (ROADMAP Z8f).
    pub(crate) async fn native(
        &self,
        request: bulkhead_native::protocol::HttpRequest,
    ) -> Result<bulkhead_native::protocol::HttpResponse, bulkhead_native::protocol::HttpError> {
        #[cfg(feature = "egress")]
        {
            self.hooks.native(request).await
        }
        #[cfg(not(feature = "egress"))]
        {
            let _ = request;
            Err(bulkhead_native::protocol::HttpError::Other(
                "this node was built without egress (cargo feature `egress`)".into(),
            ))
        }
    }
}

#[cfg(feature = "egress")]
mod hooks {
    use std::sync::{Arc, PoisonError};

    use super::client::{Error, HttpBody};
    use tokio::sync::Semaphore;

    use bulkhead_native::protocol as p;

    use super::policy::{self, Allow, Scheme};
    use super::{EgressSpec, InstanceInfo, Shared};
    use crate::metrics::AppStats;

    const SENT: usize = 0;
    const DENIED: usize = 1;
    const LIMITED: usize = 2;
    const FAILED: usize = 3;

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
        fn refuse(&self, request: &hyper::Request<HttpBody>) -> Option<&'static str> {
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

    /// The most a native app's response body carries: a frame holds it.
    const MAX_NATIVE_BODY: usize = 16 << 20;

    impl Hooks {
        /// An app's request: checked against the filter (forbidden headers
        /// too), then sent through the pool, and the whole response read.
        pub(crate) async fn native(
            &self,
            request: p::HttpRequest,
        ) -> Result<p::HttpResponse, p::HttpError> {
            use http_body_util::BodyExt;

            let invalid = |what: &str, e: &dyn std::fmt::Display| {
                p::HttpError::Invalid(format!("{what}: {e}"))
            };
            let method = hyper::Method::from_bytes(request.method.as_bytes())
                .map_err(|e| invalid("method", &e))?;
            let uri: hyper::Uri = request.url.parse().map_err(|e| invalid("url", &e))?;
            let mut builder = hyper::Request::builder().method(method).uri(uri);
            for (name, value) in &request.headers {
                let name = hyper::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| invalid("header", &e))?;
                if forbidden(&name) {
                    return Err(p::HttpError::Invalid(format!("header {name} is forbidden")));
                }
                let value = hyper::header::HeaderValue::from_bytes(&value.0)
                    .map_err(|e| invalid("header", &e))?;
                builder = builder.header(name, value);
            }
            let body: HttpBody = http_body_util::Full::new(bytes::Bytes::from(request.body.0))
                .map_err(|never| match never {})
                .boxed_unsync();
            let request_out = builder.body(body).map_err(|e| invalid("request", &e))?;
            if let Some(why) = self.refuse(&request_out) {
                tracing::debug!(app = %self.app, uri = %request_out.uri(), why, "egress request denied");
                self.stats.egress(DENIED);
                return Err(p::HttpError::Denied);
            }
            let Ok(permit) = self.limit.clone().try_acquire_owned() else {
                self.stats.egress(LIMITED);
                return Err(p::HttpError::Limited);
            };
            let config = &self.shared.config;
            let asked = request
                .timeout_ms
                .map(|ms| std::time::Duration::from_millis(ms.into()));
            let shorter = |node: std::time::Duration| asked.map_or(node, |a| a.min(node));
            let sent = self.shared.pool.send(
                request_out,
                shorter(config.first_byte_timeout),
                shorter(config.between_bytes_timeout),
                permit,
            );
            let read = async {
                let response = sent.await?;
                let status = response.status().as_u16();
                let headers = response
                    .headers()
                    .iter()
                    .filter(|(n, _)| !forbidden(n))
                    .map(|(n, v)| (n.as_str().to_string(), p::Bytes(v.as_bytes().to_vec())))
                    .collect();
                let mut body = response.into_body();
                let mut bytes = Vec::new();
                while let Some(frame) = body.frame().await {
                    if let Ok(data) = frame?.into_data() {
                        if bytes.len() + data.len() > MAX_NATIVE_BODY {
                            return Err(Error::HttpResponseBodySize);
                        }
                        bytes.extend_from_slice(&data);
                    }
                }
                Ok(p::HttpResponse {
                    status,
                    headers,
                    body: p::Bytes(bytes),
                })
            };
            let response = read.await;
            self.stats
                .egress(if response.is_ok() { SENT } else { FAILED });
            response.map_err(|e| native_error(&e))
        }
    }

    /// An egress error as the native protocol's.
    fn native_error(e: &Error) -> p::HttpError {
        match e {
            Error::DestinationIpProhibited => p::HttpError::Prohibited,
            Error::ConnectionReadTimeout | Error::ConnectionTimeout => p::HttpError::Timeout,
            Error::ConnectionRefused
            | Error::DestinationUnavailable
            | Error::Dns
            | Error::ConnectionTerminated => p::HttpError::Unreachable(format!("{e:?}")),
            Error::TlsProtocol | Error::TlsCertificate | Error::TlsAlertReceived(_) => {
                p::HttpError::Tls(format!("{e:?}"))
            }
            Error::HttpResponseBodySize => {
                p::HttpError::Other(format!("response body over {MAX_NATIVE_BODY} bytes"))
            }
            Error::HttpProtocol => p::HttpError::Other(format!("{e:?}")),
        }
    }

    /// Whether `name` is a header an app may not set or see (hop-by-hop
    /// headers and `host`).
    fn forbidden(name: &hyper::header::HeaderName) -> bool {
        use hyper::header;
        matches!(
            *name,
            header::CONNECTION
                | header::PROXY_AUTHENTICATE
                | header::PROXY_AUTHORIZATION
                | header::TRANSFER_ENCODING
                | header::UPGRADE
                | header::HOST
        ) || matches!(
            name.as_str(),
            "keep-alive" | "proxy-connection" | "http2-settings"
        )
    }
}
