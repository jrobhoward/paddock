//! HTTP ingress (ROADMAP E1): the host terminates HTTP and delivers each
//! request to an app as a call.
//!
//! An [`HttpIngress`] holds the node's route table and listens on any number
//! of addresses, each with several accept tasks. A request is matched to a
//! route by host and longest path prefix (on segment boundaries), its body
//! read up to the route's `max_body`, and sent as a call to the app that
//! owns the route; the app's pool picks the instance (`instances.routing`;
//! a route's `hash_header` fills the request's `key` field for `hash:key`).
//! The app answers from `handle-call`, now or later with `process.reply`.
//!
//! The request is a CBOR map in an `application/cbor` envelope whose
//! `schema` is [`REQUEST_SCHEMA`]:
//! `{ scheme, method, path, query, headers: [[name, value]], body: bytes, key }`
//! (`scheme` is `http` or `https`).
//! A reply whose `schema` is [`RESPONSE_SCHEMA`] is a CBOR map
//! `{ status, headers: [[name, value]], body: bytes }`; any other reply is
//! sent as `200` with the envelope's content type and body.
//!
//! Failures map to statuses: no route 404, a body over `max_body` (or the
//! app's `message_max_bytes`) 413, a body not received within the
//! listener's `body_timeout` 408, the app not running or overloaded 503
//! (overloaded with `Retry-After`), no reply within the route's timeout
//! 504, the instance exiting before replying, a malformed reply or one
//! over `max_response` 502.
//!
//! A request for an app that does not run here, but runs on the node's
//! partner now ([`HttpIngress::set_elsewhere`]), is answered as the
//! listener's [`Standby`] says (ROADMAP D11): the standby of an HA pair
//! refuses it (503), redirects it with `307 Temporary Redirect` to the same
//! path and query under the partner's URL (307 keeps the method and body,
//! so a client that follows redirects repeats a POST there), or forwards it
//! over the cluster to the app on the partner, whose pool answers it
//! ([`crate::Runtime::call_app_on`]).
//!
//! Each listener caps its open connections and the time a client may take
//! to send its headers and its body ([`IngressListen`]), so slow or idle
//! clients cannot hold the node's connections. HTTP parsing runs in the
//! node, not the sandbox (DESIGN.md, "HTTP ingress": a parser bug here is
//! a node bug; connection handlers parse in the sandbox instead).
//! Bodies are buffered whole; there is no streaming (S15: about 75% of
//! native hyper's throughput through an instance call).

#[cfg(feature = "tls")]
mod tls;

#[cfg(feature = "tls")]
pub use tls::{IngressTls, TlsCertificate};

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use bytes::Bytes;
use ciborium::Value;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{
    CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName, HeaderValue, LOCATION, RETRY_AFTER,
};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use paddock_bundle::manifest::{AppManifest, ByteSize};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};

use crate::apps::Apps;
use crate::error::HostError;
use crate::message::{CallError, Envelope};

/// The `schema` of a request envelope.
pub const REQUEST_SCHEMA: &str = "paddock/http-request";
/// The `schema` of a response envelope.
pub const RESPONSE_SCHEMA: &str = "paddock/http-response";

/// Largest request body when the manifest sets no `max_body`.
pub const DEFAULT_MAX_BODY: ByteSize = ByteSize::mib(1);
/// Reply timeout when the manifest sets no `timeout_ms`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One ingress address and its limits.
#[derive(Debug, Clone)]
pub struct IngressListen {
    /// Where to listen.
    pub address: SocketAddr,
    /// Accept tasks.
    pub acceptors: usize,
    /// Most connections open at once; more are closed on accept.
    pub max_connections: usize,
    /// Time a client has to send a request's headers, on a new connection
    /// or between requests on a kept-alive one.
    pub header_timeout: Duration,
    /// Time a client has to send a request's body (408 after).
    pub body_timeout: Duration,
    /// Largest reply body from an app, in bytes (502 above it).
    pub max_response: u64,
    /// What to do with a request for an app that runs on the partner node
    /// now, not here.
    pub standby: Standby,
    /// Serve HTTPS with these certificates; `None` serves plain HTTP.
    #[cfg(feature = "tls")]
    pub tls: Option<IngressTls>,
}

impl IngressListen {
    /// `address` with the default limits: 4 acceptors, 1,024 connections,
    /// 10 s for headers, 30 s for a body, 16 MiB replies.
    #[must_use]
    pub fn new(address: SocketAddr) -> Self {
        Self {
            address,
            acceptors: 4,
            max_connections: 1_024,
            header_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(30),
            max_response: 16 << 20,
            standby: Standby::Refuse,
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

/// What a listener does with a request for an app that runs on the
/// partner node now, not here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Standby {
    /// Answer `503 app not running`, as a load balancer's health check
    /// expects of the node that does not run the app.
    #[default]
    Refuse,
    /// `307 Temporary Redirect` to the same path and query under the
    /// partner's address for this listener, as clients reach it
    /// (`https://node2.example:8443`, no trailing `/`).
    Redirect(String),
    /// Send the request over the cluster to the app on the partner, and
    /// answer with its reply.
    Forward,
}

/// One route an app owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRoute {
    /// Host to match, without port, any case; `None` matches any host.
    pub host: Option<String>,
    /// Path prefix, matched on segment boundaries.
    pub path_prefix: String,
    /// A header whose value becomes the request's `key`.
    pub hash_header: Option<String>,
    /// Largest body accepted, in bytes.
    pub max_body: u64,
    /// Time allowed for the reply.
    pub timeout: Duration,
}

impl HttpRoute {
    /// The routes a manifest's `wasi:http/incoming-handler` scope declares
    /// (none if it declares no scope).
    #[must_use]
    pub fn from_manifest(manifest: &AppManifest) -> Vec<Self> {
        let Some(scope) = &manifest.spec.capabilities.http_incoming else {
            return Vec::new();
        };
        let max_body = scope.max_body.unwrap_or(DEFAULT_MAX_BODY).bytes();
        let timeout = scope
            .timeout_ms
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
        scope
            .routes
            .iter()
            .map(|r| Self {
                host: r.host.as_ref().map(|h| h.to_ascii_lowercase()),
                path_prefix: r.path_prefix.clone(),
                hash_header: r.hash_header.clone(),
                max_body,
                timeout,
            })
            .collect()
    }

    fn matches(&self, host: Option<&str>, path: &str) -> bool {
        let host_ok = match (&self.host, host) {
            (None, _) => true,
            (Some(want), Some(got)) => want.eq_ignore_ascii_case(got),
            (Some(_), None) => false,
        };
        host_ok && prefix_matches(&self.path_prefix, path)
    }
}

/// Whether `prefix` covers `path` on a segment boundary.
fn prefix_matches(prefix: &str, path: &str) -> bool {
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    prefix.ends_with('/') || rest.is_empty() || rest.starts_with('/')
}

/// What one ingress has served.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct IngressStats {
    /// Requests answered.
    pub requests: u64,
    /// Of those, answered with a 5xx status.
    pub server_errors: u64,
    /// Of those, matched no route.
    pub not_found: u64,
    /// Connections closed on accept, over `max_connections`.
    pub refused: u64,
    /// Of the requests, redirected to the partner node.
    pub redirected: u64,
    /// Of the requests, forwarded to the partner node.
    pub forwarded: u64,
}

#[derive(Debug, Default)]
struct Counters {
    requests: AtomicU64,
    server_errors: AtomicU64,
    not_found: AtomicU64,
    refused: AtomicU64,
    redirected: AtomicU64,
    forwarded: AtomicU64,
}

/// The node an app that does not run here runs on now, if it is the
/// partner and can be reached.
pub type Elsewhere = Arc<dyn Fn(&str) -> Option<u32> + Send + Sync>;

#[derive(Default)]
struct ElsewhereSlot(RwLock<Option<Elsewhere>>);

impl std::fmt::Debug for ElsewhereSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ElsewhereSlot")
    }
}

impl ElsewhereSlot {
    fn runs(&self, app: &str) -> Option<u32> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|f| f(app))
    }
}

/// The node's HTTP ingress. Cheap to clone; clones share the route table
/// and listeners. Dropping the last clone does not stop the listeners; call
/// [`HttpIngress::stop`].
#[derive(Debug, Clone)]
pub struct HttpIngress {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    apps: Apps,
    /// Routes by app.
    routes: RwLock<BTreeMap<String, Vec<HttpRoute>>>,
    stop: watch::Sender<bool>,
    counters: Counters,
    elsewhere: ElsewhereSlot,
}

impl HttpIngress {
    /// An ingress delivering to `apps`, with no routes and no listeners.
    #[must_use]
    pub fn new(apps: Apps) -> Self {
        Self {
            inner: Arc::new(Inner {
                apps,
                routes: RwLock::default(),
                stop: watch::channel(false).0,
                counters: Counters::default(),
                elsewhere: ElsewhereSlot::default(),
            }),
        }
    }

    /// Listen on `spec.address` with its limits. Returns the bound address
    /// (for port 0).
    ///
    /// # Errors
    ///
    /// [`HostError::Listen`] if the address cannot be bound.
    pub async fn listen(&self, spec: IngressListen) -> Result<SocketAddr, HostError> {
        let listener = TcpListener::bind(spec.address)
            .await
            .map_err(|e| HostError::Listen(format!("{}: {e}", spec.address)))?;
        let bound = listener
            .local_addr()
            .map_err(|e| HostError::Listen(e.to_string()))?;
        let listener = Arc::new(listener);
        let slots = Arc::new(Semaphore::new(spec.max_connections.max(1)));
        let spec = Arc::new(spec);
        for _ in 0..spec.acceptors.max(1) {
            tokio::spawn(accept(
                self.inner.clone(),
                listener.clone(),
                slots.clone(),
                spec.clone(),
            ));
        }
        Ok(bound)
    }

    /// Give `app` exactly `routes`, replacing any it had.
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidApps`] if another app owns a route with the same
    /// host and prefix; nothing changes then.
    pub fn set_routes(&self, app: &str, routes: Vec<HttpRoute>) -> Result<(), HostError> {
        let mut table = self
            .inner
            .routes
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for route in &routes {
            let taken = table
                .iter()
                .filter(|(owner, _)| *owner != app)
                .find_map(|(owner, rs)| {
                    rs.iter()
                        .any(|r| r.host == route.host && r.path_prefix == route.path_prefix)
                        .then_some(owner)
                });
            if let Some(owner) = taken {
                return Err(HostError::InvalidApps(format!(
                    "HTTP route {}{} belongs to {owner}",
                    route.host.as_deref().unwrap_or("*"),
                    route.path_prefix
                )));
            }
        }
        if routes.is_empty() {
            table.remove(app);
        } else {
            table.insert(app.to_string(), routes);
        }
        Ok(())
    }

    /// Withdraw `app`'s routes.
    pub fn remove_routes(&self, app: &str) {
        self.inner
            .routes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(app);
    }

    /// `app`'s routes.
    #[must_use]
    pub fn routes(&self, app: &str) -> Vec<HttpRoute> {
        self.inner
            .routes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(app)
            .cloned()
            .unwrap_or_default()
    }

    /// Tell the ingress which apps run on the partner node now, for
    /// listeners that redirect or forward them ([`Standby`]). Called for a
    /// request whose app does not run here, so it must be cheap.
    pub fn set_elsewhere(&self, elsewhere: Elsewhere) {
        *self
            .inner
            .elsewhere
            .0
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(elsewhere);
    }

    /// What this ingress has served.
    #[must_use]
    pub fn stats(&self) -> IngressStats {
        let c = &self.inner.counters;
        IngressStats {
            requests: c.requests.load(Ordering::Relaxed),
            server_errors: c.server_errors.load(Ordering::Relaxed),
            not_found: c.not_found.load(Ordering::Relaxed),
            refused: c.refused.load(Ordering::Relaxed),
            redirected: c.redirected.load(Ordering::Relaxed),
            forwarded: c.forwarded.load(Ordering::Relaxed),
        }
    }

    /// Stop accepting and close open connections.
    pub fn stop(&self) {
        self.inner.stop.send_replace(true);
    }
}

async fn accept(
    inner: Arc<Inner>,
    listener: Arc<TcpListener>,
    slots: Arc<Semaphore>,
    spec: Arc<IngressListen>,
) {
    let mut stop = inner.stop.subscribe();
    loop {
        let accepted = tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else {
            continue;
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            inner.counters.refused.fetch_add(1, Ordering::Relaxed);
            drop(stream);
            continue;
        };
        let inner = inner.clone();
        let spec = spec.clone();
        let stop = inner.stop.subscribe();
        tokio::spawn(async move {
            let _slot = slot;
            #[cfg(not(feature = "tls"))]
            serve_connection(inner, spec, stream, "http", stop).await;
            #[cfg(feature = "tls")]
            match spec.tls.clone() {
                None => serve_connection(inner, spec, stream, "http", stop).await,
                Some(tls) => {
                    // The handshake counts against the header timeout.
                    let handshake = tls.acceptor().accept(stream);
                    match tokio::time::timeout(spec.header_timeout, handshake).await {
                        Ok(Ok(stream)) => {
                            serve_connection(inner, spec, stream, "https", stop).await;
                        }
                        Ok(Err(e)) => tracing::debug!(error = %e, "TLS handshake failed"),
                        Err(_) => tracing::debug!("TLS handshake timed out"),
                    }
                }
            }
        });
    }
}

/// Serve HTTP/1.1 on one connection, plain or TLS, until it closes or the
/// ingress stops.
async fn serve_connection<IO>(
    inner: Arc<Inner>,
    spec: Arc<IngressListen>,
    io: IO,
    scheme: &'static str,
    mut stop: watch::Receiver<bool>,
) where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(|req| {
        let inner = inner.clone();
        let spec = spec.clone();
        async move { Ok::<_, Infallible>(serve(&inner, &spec, scheme, req).await) }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(spec.header_timeout)
        .serve_connection(TokioIo::new(io), service);
    tokio::pin!(conn);
    tokio::select! {
        _ = conn.as_mut() => {}
        _ = stop.wait_for(|s| *s) => conn.as_mut().graceful_shutdown(),
    }
}

type Reply = Response<Full<Bytes>>;

fn status(code: StatusCode, text: &str) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from(format!("{text}\n"))));
    *r.status_mut() = code;
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    r
}

/// `307` to `uri`'s path and query under `base`; 503 if that is not a
/// valid header value.
fn redirect(base: &str, uri: &hyper::Uri) -> Reply {
    let rest = uri.path_and_query().map_or("/", |pq| pq.as_str());
    let to = format!("{base}{rest}");
    let Ok(location) = HeaderValue::from_str(&to) else {
        return status(StatusCode::SERVICE_UNAVAILABLE, "app not running");
    };
    let mut r = status(
        StatusCode::TEMPORARY_REDIRECT,
        &format!("runs on the partner node: {to}"),
    );
    r.headers_mut().insert(LOCATION, location);
    r
}

/// The best route for a request: longest prefix, and a route naming the
/// host before one that does not.
fn pick(
    table: &BTreeMap<String, Vec<HttpRoute>>,
    host: Option<&str>,
    path: &str,
) -> Option<(String, HttpRoute)> {
    table
        .iter()
        .flat_map(|(app, rs)| rs.iter().map(move |r| (app, r)))
        .filter(|(_, r)| r.matches(host, path))
        .max_by_key(|(_, r)| (r.path_prefix.len(), r.host.is_some()))
        .map(|(app, r)| (app.clone(), r.clone()))
}

async fn serve(
    inner: &Inner,
    spec: &IngressListen,
    scheme: &'static str,
    req: Request<Incoming>,
) -> Reply {
    let reply = handle(inner, spec, scheme, req).await;
    let c = &inner.counters;
    c.requests.fetch_add(1, Ordering::Relaxed);
    if reply.status().is_server_error() {
        c.server_errors.fetch_add(1, Ordering::Relaxed);
    }
    if reply.status() == StatusCode::NOT_FOUND {
        c.not_found.fetch_add(1, Ordering::Relaxed);
    }
    if reply.status() == StatusCode::TEMPORARY_REDIRECT {
        c.redirected.fetch_add(1, Ordering::Relaxed);
    }
    reply
}

async fn handle(
    inner: &Inner,
    spec: &IngressListen,
    scheme: &'static str,
    req: Request<Incoming>,
) -> Reply {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| req.uri().host())
        .map(|h| h.split(':').next().unwrap_or(h).to_ascii_lowercase());
    let path = req.uri().path().to_string();
    let picked = {
        let table = inner.routes.read().unwrap_or_else(PoisonError::into_inner);
        pick(&table, host.as_deref(), &path)
    };
    let Some((app, route)) = picked else {
        return status(StatusCode::NOT_FOUND, "no route");
    };
    let partner = match &spec.standby {
        Standby::Refuse => None,
        _ if inner.apps.app(&app).is_some() => None,
        standby => inner.elsewhere.runs(&app).map(|node| (standby, node)),
    };
    let forward_to = match partner {
        Some((Standby::Redirect(base), _)) => return redirect(base, req.uri()),
        Some((_, node)) => Some(node),
        None => None,
    };
    let declared = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > route.max_body) {
        return status(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
    }
    let method = req.method().as_str().to_string();
    let query = req.uri().query().map(str::to_string);
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let key = route
        .hash_header
        .as_deref()
        .and_then(|h| req.headers().get(h))
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    let limit = usize::try_from(route.max_body).unwrap_or(usize::MAX);
    let read = Limited::new(req.into_body(), limit).collect();
    let body = match tokio::time::timeout(spec.body_timeout, read).await {
        Err(_) => return status(StatusCode::REQUEST_TIMEOUT, "body not received in time"),
        Ok(Ok(b)) => b.to_bytes(),
        Ok(Err(e))
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some() =>
        {
            return status(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
        }
        Ok(Err(_)) => return status(StatusCode::BAD_REQUEST, "unreadable body"),
    };
    let msg = request_envelope(scheme, &method, &path, query, &headers, &body, key);
    let result = match (forward_to, inner.apps.app(&app)) {
        (Some(node), _) => {
            inner.counters.forwarded.fetch_add(1, Ordering::Relaxed);
            inner
                .apps
                .runtime()
                .call_app_on(node, &app, msg, route.timeout)
                .await
        }
        (None, Some(target)) => target.call(msg, route.timeout).await,
        (None, None) => return status(StatusCode::SERVICE_UNAVAILABLE, "app not running"),
    };
    match result {
        Ok(reply) if u64::try_from(reply.body.len()).unwrap_or(u64::MAX) > spec.max_response => {
            tracing::debug!(
                app,
                bytes = reply.body.len(),
                "HTTP reply over max_response"
            );
            status(StatusCode::BAD_GATEWAY, "reply too large")
        }
        Ok(reply) => response(reply),
        Err(CallError::Overloaded) => {
            let mut r = status(StatusCode::SERVICE_UNAVAILABLE, "overloaded");
            r.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("1"));
            r
        }
        Err(CallError::Noproc | CallError::Noconnection) => {
            status(StatusCode::SERVICE_UNAVAILABLE, "app not running")
        }
        Err(CallError::Timeout) => status(StatusCode::GATEWAY_TIMEOUT, "no reply in time"),
        Err(CallError::TooLarge) => status(StatusCode::PAYLOAD_TOO_LARGE, "body too large"),
        Err(CallError::AccessDenied) => status(StatusCode::FORBIDDEN, "refused"),
        Err(CallError::Exited(reason)) => {
            tracing::debug!(app, %reason, "HTTP request: instance exited");
            status(StatusCode::BAD_GATEWAY, "the app failed")
        }
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

/// The call a request becomes.
fn request_envelope(
    scheme: &str,
    method: &str,
    path: &str,
    query: Option<String>,
    headers: &[(String, String)],
    body: &[u8],
    key: Option<String>,
) -> Envelope {
    let headers = headers
        .iter()
        .map(|(k, v)| Value::Array(vec![text(k), text(v)]))
        .collect();
    let mut map = vec![
        (text("scheme"), text(scheme)),
        (text("method"), text(method)),
        (text("path"), text(path)),
        (text("query"), query.map_or(Value::Null, Value::Text)),
        (text("headers"), Value::Array(headers)),
        (text("body"), Value::Bytes(body.to_vec())),
    ];
    if let Some(key) = key {
        map.push((text("key"), Value::Text(key)));
    }
    let mut out = Vec::new();
    // Writing a CBOR value to a Vec cannot fail.
    let _ = ciborium::into_writer(&Value::Map(map), &mut out);
    Envelope {
        content_type: "application/cbor".into(),
        schema: Some(REQUEST_SCHEMA.into()),
        body: out,
    }
}

/// The HTTP response an app's reply becomes.
fn response(reply: Envelope) -> Reply {
    if reply.schema.as_deref() != Some(RESPONSE_SCHEMA) {
        let mut r = Response::new(Full::new(Bytes::from(reply.body)));
        if let Ok(v) = HeaderValue::from_str(&reply.content_type) {
            r.headers_mut().insert(CONTENT_TYPE, v);
        }
        return r;
    }
    match parse_response(&reply.body) {
        Some(r) => r,
        None => status(StatusCode::BAD_GATEWAY, "malformed response from the app"),
    }
}

/// For the fuzz targets in `fuzz/` only: read an app's reply envelope
/// body as an HTTP response; the status code if it is one.
#[cfg(fuzzing)]
#[doc(hidden)]
#[must_use]
pub fn fuzz_response(body: &[u8]) -> Option<u16> {
    parse_response(body).map(|r| r.status().as_u16())
}

fn parse_response(body: &[u8]) -> Option<Reply> {
    let value: Value = ciborium::from_reader(body).ok()?;
    let map = value.as_map()?;
    let field = |name: &str| {
        map.iter()
            .find(|(k, _)| k.as_text() == Some(name))
            .map(|(_, v)| v)
    };
    let code = field("status")
        .and_then(Value::as_integer)
        .and_then(|i| u16::try_from(i).ok())
        .unwrap_or(200);
    let mut r = Response::new(Full::new(Bytes::from(
        field("body")
            .and_then(Value::as_bytes)
            .cloned()
            .unwrap_or_default(),
    )));
    *r.status_mut() = StatusCode::from_u16(code).ok()?;
    if let Some(headers) = field("headers").and_then(Value::as_array) {
        for pair in headers {
            let pair = pair.as_array()?;
            let (name, value) = (pair.first()?.as_text()?, pair.get(1)?.as_text()?);
            r.headers_mut().append(
                HeaderName::from_bytes(name.as_bytes()).ok()?,
                HeaderValue::from_str(value).ok()?,
            );
        }
    }
    Some(r)
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod http_tests;
