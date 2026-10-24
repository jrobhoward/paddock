//! The admin API for operators on other machines: the same
//! routes as the admin socket, over TCP with TLS 1.3 and pinned keys.
//!
//! The node presents its key (`[admin] key`, or `[cluster] key`); an
//! operator connects only with a key the operators file lists, and has
//! every right the admin socket gives. Each connection is logged with the
//! operator's name, and each request that changes something (anything but
//! `GET`) is written to the audit log with it. The operators file is read
//! again on SIGHUP and `paddock reload-tls`; connections already open keep
//! going.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use paddock_host::transport::quic::{PublicKey, admin_server_tls, parse_operators, presented_key};
use serde_json::json;

use super::Node;
use crate::node::error::NodeError;

/// How long a client may take to finish the TLS handshake.
const HANDSHAKE: Duration = Duration::from_secs(10);

/// Operators by name, as last read.
pub(crate) type Operators = Arc<RwLock<BTreeMap<String, Vec<PublicKey>>>>;

/// Read the operators file.
pub(crate) fn read_operators(path: &Path) -> Result<BTreeMap<String, Vec<PublicKey>>, NodeError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| NodeError::Invalid(format!("admin operators {}: {e}", path.display())))?;
    parse_operators(&text)
        .map_err(|e| NodeError::Invalid(format!("admin operators {}: {e}", path.display())))
}

/// The name of the operator whose key `key` is.
fn operator_of(operators: &Operators, key: &PublicKey) -> Option<String> {
    operators
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(_, keys)| keys.contains(key))
        .map(|(name, _)| name.clone())
}

impl Node {
    /// Read the operators file again, if there is a remote admin API.
    pub(super) fn reload_operators(&self) -> Result<(), NodeError> {
        let Some(admin) = &self.config.admin else {
            return Ok(());
        };
        let fresh = read_operators(&admin.operators)?;
        *self
            .operators
            .write()
            .unwrap_or_else(PoisonError::into_inner) = fresh;
        Ok(())
    }

    /// Start the remote admin API on `[admin] address`, if configured; it
    /// stops with the node.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] if the key or operators file cannot be read,
    /// or the address cannot be bound.
    pub async fn start_remote_admin(self: &Arc<Self>) -> Result<(), NodeError> {
        let Some(admin) = &self.config.admin else {
            return Ok(());
        };
        let key = admin
            .key
            .as_ref()
            .or(self.config.cluster.key.as_ref())
            .ok_or_else(|| NodeError::Invalid("[admin] needs a key, or [cluster] key".into()))?;
        self.reload_operators()?;
        let operators = self.operators.clone();
        let accepts = Arc::new(move |k: &PublicKey| operator_of(&operators, k).is_some());
        let tls = admin_server_tls(key, accepts).map_err(|e| NodeError::Invalid(e.to_string()))?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind(admin.address)
            .await
            .map_err(|e| NodeError::Invalid(format!("admin: {}: {e}", admin.address)))?;
        tracing::info!(address = %admin.address, "remote admin API listening");
        tokio::spawn(serve(self.clone(), listener, acceptor));
        Ok(())
    }
}

async fn serve(
    node: Arc<Node>,
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
) {
    let mut stop = node.stop.subscribe();
    loop {
        let (stream, peer) = tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(error = %e, "remote admin accept failed");
                    continue;
                }
            },
        };
        let (node, acceptor) = (node.clone(), acceptor.clone());
        tokio::spawn(async move {
            let tls = match tokio::time::timeout(HANDSHAKE, acceptor.accept(stream)).await {
                Ok(Ok(tls)) => tls,
                Ok(Err(e)) => {
                    tracing::info!(%peer, error = %e, "remote admin handshake refused");
                    return;
                }
                Err(_) => {
                    tracing::info!(%peer, "remote admin handshake timed out");
                    return;
                }
            };
            let key = presented_key(tls.get_ref().1.peer_certificates());
            let Some(operator) = key.and_then(|k| operator_of(&node.operators, &k)) else {
                // The verifier accepted the key, but a reload removed it since.
                return;
            };
            tracing::info!(%peer, operator, "remote admin connection");
            let service = hyper::service::service_fn(move |req: hyper::Request<_>| {
                let node = node.clone();
                let operator = operator.clone();
                async move {
                    if req.method() != hyper::Method::GET {
                        node.audit.record(
                            "remote_admin",
                            json!({
                                "operator": operator,
                                "method": req.method().as_str(),
                                "path": req.uri().path(),
                            }),
                        );
                    }
                    Ok::<_, std::convert::Infallible>(crate::node::server::handle(&node, req).await)
                }
            });
            let io = hyper_util::rt::TokioIo::new(tls);
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                tracing::debug!(error = %e, "remote admin connection ended");
            }
        });
    }
}
