//! The admin API server: HTTP/1.1 on a Unix socket created with mode 0600.

// Without Unix sockets (Windows, Phase 1) `serve` only returns an error, so
// the router below is unreachable there.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use crate::api::{ApiError, RolloutParams, Target};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use paddock_host::Envelope;
use serde::Serialize;

use super::daemon::Node;
use super::error::NodeError;

/// Largest request body accepted (a bundle).
pub const MAX_BODY: usize = 512 << 20;

type Reply = Response<Full<Bytes>>;

/// Serve the admin API on the node's socket until `shutdown` completes.
/// Removes a stale socket file first.
///
/// # Errors
///
/// The I/O error if the socket cannot be bound.
#[cfg(unix)]
pub async fn serve(node: Arc<Node>, shutdown: impl Future<Output = ()>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let path = node.socket();
    check_socket_path(&path)?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = tokio::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %path.display(), "admin API listening");
    tokio::pin!(shutdown);
    loop {
        let stream = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::warn!(error = %e, "admin accept failed");
                    continue;
                }
            },
        };
        let node = node.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let node = node.clone();
                async move { Ok::<_, Infallible>(handle(&node, req).await) }
            });
            let io = hyper_util::rt::TokioIo::new(stream);
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                tracing::debug!(error = %e, "admin connection ended");
            }
        });
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Longest Unix socket path, in bytes, without the terminating NUL
/// (`sun_path` is 108 bytes on Linux and 104 on macOS and the BSDs).
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = if cfg!(target_os = "linux") { 107 } else { 103 };

/// Refuse a socket path the OS cannot bind, naming the fix.
#[cfg(unix)]
fn check_socket_path(path: &std::path::Path) -> std::io::Result<()> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "admin socket path {} is {len} bytes; Unix sockets allow at most \
                 {MAX_SOCKET_PATH}. Set admin_socket in the node config to a shorter path",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// The admin API needs Unix domain sockets; this platform has none.
///
/// # Errors
///
/// Always [`std::io::ErrorKind::Unsupported`].
#[cfg(not(unix))]
pub async fn serve(_node: Arc<Node>, _shutdown: impl Future<Output = ()>) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the admin API needs Unix domain sockets; Windows support is not in Phase 1",
    ))
}

fn json<T: Serialize>(status: StatusCode, value: &T) -> Reply {
    let body = serde_json::to_vec(value).unwrap_or_default();
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

/// Serve `GET /metrics` (and `GET /`) with `render`'s text on `listener`
/// until `stop` turns true. Unauthenticated, like most Prometheus
/// endpoints: bind it to loopback or a management network.
pub(crate) async fn serve_metrics(
    listener: tokio::net::TcpListener,
    render: impl Fn() -> String + Send + Sync + 'static,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let render = Arc::new(render);
    loop {
        let accepted = tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else {
            continue;
        };
        let render = render.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                let render = render.clone();
                async move {
                    let reply = match (req.method(), req.uri().path()) {
                        (&Method::GET, "/metrics" | "/") => metrics_text(render()),
                        _ => {
                            let mut r =
                                Response::new(Full::new(Bytes::from_static(b"GET /metrics\n")));
                            *r.status_mut() = StatusCode::NOT_FOUND;
                            r
                        }
                    };
                    Ok::<_, Infallible>(reply)
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await;
        });
    }
}

/// A Prometheus text body.
pub(crate) fn metrics_text(text: String) -> Reply {
    let mut response = Response::new(Full::new(Bytes::from(text)));
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    response
}

fn error(e: &NodeError) -> Reply {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    json(
        status,
        &ApiError {
            kind: e.kind().to_string(),
            error: e.to_string(),
        },
    )
}

fn respond<T: Serialize>(result: Result<T, NodeError>) -> Reply {
    match result {
        Ok(value) => json(StatusCode::OK, &value),
        Err(e) => error(&e),
    }
}

/// `key=value` pairs from a query string.
fn query(req: &Request<Incoming>) -> Vec<(String, String)> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, "true"));
            (k.to_string(), v.to_string())
        })
        .collect()
}

fn param<T: std::str::FromStr>(q: &[(String, String)], key: &str) -> Result<Option<T>, NodeError> {
    match q.iter().find(|(k, _)| k == key) {
        None => Ok(None),
        Some((_, v)) => v
            .parse()
            .map(Some)
            .map_err(|_| NodeError::Invalid(format!("query parameter {key}={v} is not valid"))),
    }
}

fn rollout(q: &[(String, String)]) -> Result<RolloutParams, NodeError> {
    Ok(RolloutParams {
        approve: param::<bool>(q, "approve")?.unwrap_or(false),
        batch: param(q, "batch")?,
        observe_ms: param(q, "observe_ms")?,
        max_restarts: param(q, "max_restarts")?,
        restart: param::<bool>(q, "restart")?.unwrap_or(false),
    })
}

async fn body(req: Request<Incoming>) -> Result<(String, Vec<u8>), NodeError> {
    let content_type = req
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = Limited::new(req.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|e| NodeError::Invalid(format!("request body: {e}")))?
        .to_bytes();
    Ok((content_type, bytes.to_vec()))
}

pub(crate) async fn handle(node: &Node, req: Request<Incoming>) -> Reply {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let q = query(&req);
    match (&method, segments.as_slice()) {
        (&Method::GET, ["v1", "apps"]) => json(StatusCode::OK, &node.list()),
        (&Method::POST, ["v1", "apps"]) => deploy(node, req).await,
        (&Method::GET, ["v1", "apps", app]) => respond(node.status(app)),
        (&Method::DELETE, ["v1", "apps", app]) => respond(node.remove(app).await),
        (&Method::POST, ["v1", "apps", app, "stop"]) => respond(node.stop(app).await),
        (&Method::POST, ["v1", "apps", app, "start"]) => respond(node.start(app).await),
        (&Method::POST, ["v1", "apps", app, "rollback"]) => match rollout(&q) {
            Ok(p) => respond(node.rollback(app, &p).await),
            Err(e) => error(&e),
        },
        (&Method::POST, ["v1", "apps", app, "upgrade"]) => upgrade(node, app, &q, req).await,
        (
            &Method::POST,
            [
                "v1",
                kind @ ("apps" | "instances" | "names"),
                id,
                verb @ ("call" | "cast"),
            ],
        ) => send(node, kind, id, verb, &q, req).await,
        (&Method::POST, ["v1", "apps", app, "log-level"]) => {
            match q.iter().find(|(k, _)| k == "level") {
                Some((_, level)) => respond(node.set_log_level(app, level)),
                None => error(&NodeError::Invalid("level is required".into())),
            }
        }
        (&Method::GET, ["v1", "instances", pid, "state"]) => respond(node.state(pid).await),
        (&Method::GET, ["v1", "apps", app, "logs"]) => match param::<usize>(&q, "lines") {
            Ok(lines) => respond(node.logs(app, lines.unwrap_or(100))),
            Err(e) => error(&e),
        },
        (&Method::GET, ["v1", "instances"]) => json(StatusCode::OK, &node.ps()),
        (&Method::GET, ["v1", "metrics"]) => metrics_text(node.metrics()),
        (&Method::GET, ["v1", "cluster"]) => json(StatusCode::OK, &node.members()),
        (&Method::POST, ["v1", "tls", "reload"]) => respond(node.reload_tls()),
        (&Method::GET, ["v1", "failover"]) => respond(node.failover()),
        (&Method::POST, ["v1", "failover", "confirm"]) => respond(node.confirm_failover()),
        (&Method::POST, ["v1", "instances", pid, "kill"]) => respond(node.kill(pid).await),
        _ => error(&NodeError::NotFound(format!(
            "no route for {method} {path}"
        ))),
    }
}

async fn deploy(node: &Node, req: Request<Incoming>) -> Reply {
    match body(req).await {
        Ok((_, bundle)) => respond(node.deploy(&bundle).await),
        Err(e) => error(&e),
    }
}

async fn upgrade(node: &Node, app: &str, q: &[(String, String)], req: Request<Incoming>) -> Reply {
    match (rollout(q), body(req).await) {
        (Ok(p), Ok((_, bundle))) => respond(node.upgrade(app, &bundle, &p).await),
        (Err(e), _) | (_, Err(e)) => error(&e),
    }
}

/// `call` or `cast` to an app, an instance by pid, or a registered name.
async fn send(
    node: &Node,
    kind: &str,
    id: &str,
    verb: &str,
    q: &[(String, String)],
    req: Request<Incoming>,
) -> Reply {
    let target = match kind {
        "apps" => Target::App(id.to_string()),
        "instances" => Target::Pid(id.to_string()),
        _ => Target::Name(id.to_string()),
    };
    if verb == "call" {
        return call(node, &target, q, req).await;
    }
    match body(req).await {
        Ok((content_type, bytes)) => {
            respond(node.cast_to(&target, Envelope::new(content_type, bytes)))
        }
        Err(e) => error(&e),
    }
}

async fn call(
    node: &Node,
    target: &Target,
    q: &[(String, String)],
    req: Request<Incoming>,
) -> Reply {
    let timeout = match param::<u64>(q, "timeout_ms") {
        Ok(t) => Duration::from_millis(t.unwrap_or(5_000)),
        Err(e) => return error(&e),
    };
    let (content_type, bytes) = match body(req).await {
        Ok(b) => b,
        Err(e) => return error(&e),
    };
    match node
        .call_to(target, Envelope::new(content_type, bytes), timeout)
        .await
    {
        Ok(reply) => {
            let mut response = Response::new(Full::new(Bytes::from(reply.body)));
            if let Ok(v) = hyper::header::HeaderValue::from_str(&reply.content_type) {
                response
                    .headers_mut()
                    .insert(hyper::header::CONTENT_TYPE, v);
            }
            response
        }
        Err(e) => error(&e),
    }
}
