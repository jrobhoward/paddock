//! A client for the admin API over a Unix socket.

use std::path::{Path, PathBuf};

use hyper::{Method, StatusCode};
use serde::de::DeserializeOwned;

use super::{
    ApiError, AppDetail, AppSummary, DeployResponse, Done, InstanceInfo, LogLine, RolloutParams,
    StateResponse, UpgradeResponse,
};

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
    /// This platform has no Unix sockets.
    #[error("the admin API needs Unix domain sockets, which this platform lacks")]
    Unsupported,
}

/// A client bound to one node's admin socket.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    /// A client for the node listening on `socket`.
    #[must_use]
    pub fn new(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_path_buf(),
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
        self.json(
            Method::POST,
            &format!("/v1/apps/{app}/cast"),
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
        let path = format!("/v1/apps/{app}/call?timeout_ms={timeout_ms}");
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

    #[cfg(unix)]
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<(&str, Vec<u8>)>,
    ) -> Result<(StatusCode, String, Vec<u8>), ClientError> {
        use bytes::Bytes;
        use http_body_util::{BodyExt, Full};
        use hyper::Request;

        let http = |e: &dyn std::fmt::Display| ClientError::Http(e.to_string());
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(|source| ClientError::Connect {
                path: self.socket.clone(),
                source,
            })?;
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
        let response = sender.send_request(request).await.map_err(|e| http(&e))?;
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

    #[cfg(not(unix))]
    async fn send(
        &self,
        _method: Method,
        _path: &str,
        _body: Option<(&str, Vec<u8>)>,
    ) -> Result<(StatusCode, String, Vec<u8>), ClientError> {
        Err(ClientError::Unsupported)
    }
}

fn error_from(body: &[u8]) -> ClientError {
    match serde_json::from_slice::<ApiError>(body) {
        Ok(e) => ClientError::Api(e),
        Err(_) => ClientError::Decode(String::from_utf8_lossy(body).into_owned()),
    }
}
