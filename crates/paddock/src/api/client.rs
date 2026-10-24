//! A client for the admin API: over the node's Unix socket, or (with the
//! `node` feature) over TLS to its remote admin API (`[admin]`).

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Method, Response, StatusCode};
use serde::de::DeserializeOwned;

use super::{
    ApiError, AppDetail, AppSummary, DeployResponse, Done, Event, FailoverInfo, InstanceInfo,
    LogLine, MemberInfo, ReleaseInfo, RolloutParams, StateResponse, Target, UpgradeResponse,
};

/// How long a stream may go without a line, keepalives included, before
/// [`Follow::next`] gives up on the node: three of its keepalives.
const SILENCE: Duration = Duration::from_secs(45);

/// A stream from the admin API ([`Client::follow_logs`],
/// [`Client::events`]): one `T` per line.
pub struct Follow<T> {
    body: Incoming,
    buf: Vec<u8>,
    _records: PhantomData<fn() -> T>,
}

impl<T> std::fmt::Debug for Follow<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Follow")
            .field("buffered", &self.buf.len())
            .finish_non_exhaustive()
    }
}

impl<T: DeserializeOwned> Follow<T> {
    /// The next record, or `None` once the node ends the stream (it shuts
    /// down).
    ///
    /// # Errors
    ///
    /// [`ClientError::Http`] if the connection fails, or nothing (not even a
    /// keepalive) arrives for 45 seconds; [`ClientError::Decode`] for a line
    /// that is not a `T`.
    pub async fn next(&mut self) -> Result<Option<T>, ClientError> {
        loop {
            if let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=end).collect();
                let line = line[..end].trim_ascii();
                if line.is_empty() {
                    continue;
                }
                return serde_json::from_slice(line)
                    .map(Some)
                    .map_err(|e| ClientError::Decode(e.to_string()));
            }
            let frame = tokio::time::timeout(SILENCE, self.body.frame())
                .await
                .map_err(|_| {
                    ClientError::Http(format!("nothing from the node for {} s", SILENCE.as_secs()))
                })?;
            match frame {
                None => return Ok(None),
                Some(Err(e)) => return Err(ClientError::Http(e.to_string())),
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.buf.extend_from_slice(&data);
                    }
                }
            }
        }
    }
}

/// Why a request failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// The node is not reachable on the socket.
    #[error("cannot reach the node at {path}: {source}")]
    Connect {
        /// The socket path.
        path: PathBuf,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The request failed in transit.
    #[error("request failed: {0}")]
    Http(String),
    /// The node answered with an error.
    #[error("{} ({})", .0.error, .0.kind)]
    Api(ApiError),
    /// The node's answer was not what the client expected.
    #[error("unexpected response: {0}")]
    Decode(String),
    /// The node is not reachable at its remote admin address, or the TLS
    /// handshake failed (an unlisted key on either side).
    #[error("cannot reach the node at {address}: {source}")]
    RemoteConnect {
        /// The address.
        address: String,
        /// The I/O or TLS error.
        #[source]
        source: std::io::Error,
    },
    /// This platform has no Unix sockets.
    #[error("the admin API needs Unix domain sockets, which this platform lacks")]
    Unsupported,
}

/// A client bound to one node's admin API.
#[derive(Clone)]
pub struct Client {
    socket: PathBuf,
    #[cfg(feature = "tls")]
    remote: Option<Remote>,
}

/// A node's remote admin address and the TLS to reach it with.
#[cfg(feature = "tls")]
#[derive(Clone)]
struct Remote {
    address: String,
    tls: tokio_rustls::TlsConnector,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Client");
        d.field("socket", &self.socket);
        #[cfg(feature = "tls")]
        d.field("remote", &self.remote.as_ref().map(|r| r.address.clone()));
        d.finish()
    }
}

impl Client {
    /// A client for the node listening on `socket`.
    #[must_use]
    pub fn new(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_path_buf(),
            #[cfg(feature = "tls")]
            remote: None,
        }
    }

    /// A client for the node whose remote admin API listens at `address`
    /// (`host:port`), reached with `tls` (from
    /// `paddock_host::transport::quic::admin_client_tls`: the operator's
    /// key, and the node's pinned key).
    #[cfg(feature = "tls")]
    #[must_use]
    pub fn remote(
        address: impl Into<String>,
        tls: std::sync::Arc<tokio_rustls::rustls::ClientConfig>,
    ) -> Self {
        Self {
            socket: PathBuf::new(),
            remote: Some(Remote {
                address: address.into(),
                tls: tokio_rustls::TlsConnector::from(tls),
            }),
        }
    }

    /// The socket this client talks to.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// List apps.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn list(&self) -> Result<Vec<AppSummary>, ClientError> {
        self.json(Method::GET, "/v1/apps", None).await
    }

    /// One app in detail.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn status(&self, app: &str) -> Result<AppDetail, ClientError> {
        self.json(Method::GET, &format!("/v1/apps/{app}"), None)
            .await
    }

    /// Deploy a bundle.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn deploy(&self, bundle: Vec<u8>) -> Result<DeployResponse, ClientError> {
        self.json(
            Method::POST,
            "/v1/apps",
            Some(("application/octet-stream", bundle)),
        )
        .await
    }

    /// Upgrade an app to a bundle.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn upgrade(
        &self,
        app: &str,
        bundle: Vec<u8>,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, ClientError> {
        let path = format!("/v1/apps/{app}/upgrade{}", params.query());
        self.json(
            Method::POST,
            &path,
            Some(("application/octet-stream", bundle)),
        )
        .await
    }

    /// Roll an app back to its previous version.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn rollback(
        &self,
        app: &str,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, ClientError> {
        let path = format!("/v1/apps/{app}/rollback{}", params.query());
        self.json(Method::POST, &path, None).await
    }

    /// Stop an app (it stays deployed).
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn stop(&self, app: &str) -> Result<Done, ClientError> {
        self.json(Method::POST, &format!("/v1/apps/{app}/stop"), None)
            .await
    }

    /// Start a stopped app.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn start(&self, app: &str) -> Result<Done, ClientError> {
        self.json(Method::POST, &format!("/v1/apps/{app}/start"), None)
            .await
    }

    /// Stop and undeploy an app.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn remove(&self, app: &str) -> Result<Done, ClientError> {
        self.json(Method::DELETE, &format!("/v1/apps/{app}"), None)
            .await
    }

    /// Kill an instance; its supervisor restarts it.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn kill(&self, pid: &str) -> Result<Done, ClientError> {
        self.json(Method::POST, &format!("/v1/instances/{pid}/kill"), None)
            .await
    }

    /// An instance's state, from its `save-snapshot`. If `save-snapshot`
    /// traps, the instance exits and its supervisor restarts it.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn state(&self, pid: &str) -> Result<StateResponse, ClientError> {
        self.json(Method::GET, &format!("/v1/instances/{pid}/state"), None)
            .await
    }

    /// Set an app's log level (`trace` to `critical`), or `None` to return
    /// to each instance's own level.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn set_log_level(&self, app: &str, level: Option<&str>) -> Result<Done, ClientError> {
        let level = level.unwrap_or("reset");
        self.json(
            Method::POST,
            &format!("/v1/apps/{app}/log-level?level={level}"),
            None,
        )
        .await
    }

    /// Cast to an app (routed to one of its instances).
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn cast(
        &self,
        app: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Done, ClientError> {
        self.cast_to(&Target::App(app.to_string()), content_type, body)
            .await
    }

    /// Cast to an app, an instance by pid, or a registered name.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn cast_to(
        &self,
        target: &Target,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Done, ClientError> {
        self.json(
            Method::POST,
            &target.path("cast"),
            Some((content_type, body)),
        )
        .await
    }

    /// Every running instance.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn ps(&self) -> Result<Vec<InstanceInfo>, ClientError> {
        self.json(Method::GET, "/v1/instances", None).await
    }

    /// Reload every HTTPS ingress's certificates from their files.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; an API error names each ingress that failed.
    pub async fn reload_tls(&self) -> Result<Done, ClientError> {
        self.json(Method::POST, "/v1/tls/reload", None).await
    }

    /// The cluster's members as the node knows them.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn members(&self) -> Result<Vec<MemberInfo>, ClientError> {
        self.json(Method::GET, "/v1/cluster", None).await
    }

    /// The node's failover standing.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; not found for a node without `[failover]`.
    pub async fn failover(&self) -> Result<FailoverInfo, ClientError> {
        self.json(Method::GET, "/v1/failover", None).await
    }

    /// The node's failover standing once its leadership has changed since
    /// generation `after`, or after `wait` (at most 60 s) without one: a
    /// long poll.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; not found for a node without `[failover]`.
    pub async fn failover_after(
        &self,
        after: u64,
        wait: std::time::Duration,
    ) -> Result<FailoverInfo, ClientError> {
        let path = format!("/v1/failover?after={after}&wait_ms={}", wait.as_millis());
        self.json(Method::GET, &path, None).await
    }

    /// Change a blue-green upgrade under way: give the new version
    /// `percent` of the app's traffic, or with `promote` retire the old
    /// version, or with `drop` stop the new one. Exactly one of the three.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; invalid when no new version runs beside the app.
    pub async fn traffic(
        &self,
        app: &str,
        percent: Option<u8>,
        promote: bool,
        drop: bool,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, ClientError> {
        let change = match (percent, promote, drop) {
            (Some(p), _, _) => format!("percent={p}"),
            (None, true, _) => "promote=true".to_string(),
            _ => "drop=true".to_string(),
        };
        let rest = params.query();
        let path = if rest.is_empty() {
            format!("/v1/apps/{app}/traffic?{change}")
        } else {
            format!("/v1/apps/{app}/traffic{rest}&{change}")
        };
        self.json(Method::POST, &path, None).await
    }

    /// The versions the node speaks and keeps, and each connected
    /// member's.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn release(&self) -> Result<ReleaseInfo, ClientError> {
        self.json(Method::GET, "/v1/release", None).await
    }

    /// Raise the pair's version for what is kept to the node's build's,
    /// the last step of a release.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; not found for a node without `[failover]`,
    /// conflict when it is refused (no replication, the partner or the
    /// witness not connected at the node's version).
    pub async fn finalize_release(&self) -> Result<Done, ClientError> {
        self.json(Method::POST, "/v1/release/finalize", None).await
    }

    /// A planned switchover: this primary stops its singletons gracefully
    /// and hands over to its standby.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; not found for a node without `[failover]`,
    /// conflict when it is refused (not the primary, no confirming standby,
    /// no witness).
    pub async fn switchover(&self) -> Result<Done, ClientError> {
        self.json(Method::POST, "/v1/failover/switchover", None)
            .await
    }

    /// Confirm that the partner is gone, so this standby may promote
    /// itself.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; not found for a node without `[failover]`.
    pub async fn confirm_failover(&self) -> Result<Done, ClientError> {
        self.json(Method::POST, "/v1/failover/confirm", None).await
    }

    /// The node's metrics, in the Prometheus text format.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn metrics(&self) -> Result<String, ClientError> {
        let (status, _, bytes) = self.send(Method::GET, "/v1/metrics", None).await?;
        if !status.is_success() {
            return Err(error_from(&bytes));
        }
        String::from_utf8(bytes).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// An app's most recent log records, oldest first.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn logs(&self, app: &str, lines: usize) -> Result<Vec<LogLine>, ClientError> {
        self.json(
            Method::GET,
            &format!("/v1/apps/{app}/logs?lines={lines}"),
            None,
        )
        .await
    }

    /// An app's last `lines` log records, then each new one as it is
    /// logged.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`]; not found for an app that is not deployed.
    pub async fn follow_logs(
        &self,
        app: &str,
        lines: usize,
    ) -> Result<Follow<LogLine>, ClientError> {
        self.follow(&format!("/v1/apps/{app}/logs?lines={lines}&follow=true"))
            .await
    }

    /// The node's events from now on ([`Event`]).
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn events(&self) -> Result<Follow<Event>, ClientError> {
        self.follow("/v1/events").await
    }

    async fn follow<T>(&self, path: &str) -> Result<Follow<T>, ClientError> {
        let response = self.open(Method::GET, path, None).await?;
        if !response.status().is_success() {
            return Err(error_from(&collect(response).await?.2));
        }
        Ok(Follow {
            body: response.into_body(),
            buf: Vec::new(),
            _records: PhantomData,
        })
    }

    /// Call an app (routed to one of its instances) and return the reply's
    /// content type and body.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn call(
        &self,
        app: &str,
        content_type: &str,
        body: Vec<u8>,
        timeout_ms: u64,
    ) -> Result<(String, Vec<u8>), ClientError> {
        self.call_to(
            &Target::App(app.to_string()),
            content_type,
            body,
            timeout_ms,
        )
        .await
    }

    /// Call an app, an instance by pid, or a registered name, and return
    /// the reply's content type and body.
    ///
    /// # Errors
    ///
    /// Any [`ClientError`].
    pub async fn call_to(
        &self,
        target: &Target,
        content_type: &str,
        body: Vec<u8>,
        timeout_ms: u64,
    ) -> Result<(String, Vec<u8>), ClientError> {
        let path = format!("{}?timeout_ms={timeout_ms}", target.path("call"));
        let (status, content_type, body) = self
            .send(Method::POST, &path, Some((content_type, body)))
            .await?;
        if !status.is_success() {
            return Err(error_from(&body));
        }
        Ok((content_type, body))
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<(&str, Vec<u8>)>,
    ) -> Result<T, ClientError> {
        let (status, _, bytes) = self.send(method, path, body).await?;
        if !status.is_success() {
            return Err(error_from(&bytes));
        }
        serde_json::from_slice(&bytes).map_err(|e| ClientError::Decode(e.to_string()))
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<(&str, Vec<u8>)>,
    ) -> Result<(StatusCode, String, Vec<u8>), ClientError> {
        collect(self.open(method, path, body).await?).await
    }

    /// Send a request and return the response with its body unread.
    async fn open(
        &self,
        method: Method,
        path: &str,
        body: Option<(&str, Vec<u8>)>,
    ) -> Result<Response<Incoming>, ClientError> {
        #[cfg(feature = "tls")]
        if let Some(remote) = &self.remote {
            let failed = |source: std::io::Error| ClientError::RemoteConnect {
                address: remote.address.clone(),
                source,
            };
            let stream = tokio::net::TcpStream::connect(&remote.address)
                .await
                .map_err(failed)?;
            // Raw public keys: the name is not checked, the pinned key is.
            let name = tokio_rustls::rustls::pki_types::ServerName::try_from("paddock")
                .map_err(|e| ClientError::Http(e.to_string()))?;
            let tls = remote.tls.connect(name, stream).await.map_err(failed)?;
            return exchange(tls, method, path, body).await;
        }
        self.send_local(method, path, body).await
    }

    #[cfg(unix)]
    async fn send_local(
        &self,
        method: Method,
        path: &str,
        body: Option<(&str, Vec<u8>)>,
    ) -> Result<Response<Incoming>, ClientError> {
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(|source| ClientError::Connect {
                path: self.socket.clone(),
                source,
            })?;
        exchange(stream, method, path, body).await
    }

    #[cfg(not(unix))]
    async fn send_local(
        &self,
        _method: Method,
        _path: &str,
        _body: Option<(&str, Vec<u8>)>,
    ) -> Result<Response<Incoming>, ClientError> {
        Err(ClientError::Unsupported)
    }
}

/// One HTTP/1.1 request over `stream`, and its response with the body
/// unread.
async fn exchange<S>(
    stream: S,
    method: Method,
    path: &str,
    body: Option<(&str, Vec<u8>)>,
) -> Result<Response<Incoming>, ClientError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::Request;

    let http = |e: &dyn std::fmt::Display| ClientError::Http(e.to_string());
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|e| http(&e))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "paddock");
    let payload = match body {
        Some((content_type, bytes)) => {
            request = request.header(hyper::header::CONTENT_TYPE, content_type);
            Bytes::from(bytes)
        }
        None => Bytes::new(),
    };
    let request = request.body(Full::new(payload)).map_err(|e| http(&e))?;
    sender.send_request(request).await.map_err(|e| http(&e))
}

/// A response's status, content type and whole body.
async fn collect(
    response: Response<Incoming>,
) -> Result<(StatusCode, String, Vec<u8>), ClientError> {
    let http = |e: &dyn std::fmt::Display| ClientError::Http(e.to_string());
    let status = response.status();
    let content_type = response
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| http(&e))?
        .to_bytes();
    Ok((status, content_type, bytes.to_vec()))
}

fn error_from(body: &[u8]) -> ClientError {
    match serde_json::from_slice::<ApiError>(body) {
        Ok(e) => ClientError::Api(e),
        Err(_) => ClientError::Decode(String::from_utf8_lossy(body).into_owned()),
    }
}
