//! The node's HTTP client for egress: one connection pool for every app
//! (spike S15: wasmtime's own sender opened a connection per request),
//! TLS through rustls with the system's roots and node config's CA file,
//! and a resolver that refuses names resolving to loopback or link-local
//! addresses.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::{GaiResolver, Name};
use hyper_util::rt::{TokioExecutor, TokioTimer};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Sleep;
use wasmtime_wasi_http::{Error, WasiBody};

use super::policy::guarded;
use crate::config::Egress;

/// The pool every app's requests go through.
pub(crate) struct Pool {
    client: Client<HttpsConnector<HttpConnector<Guard>>, WasiBody>,
}

impl Pool {
    /// A pool as node config describes it.
    pub(crate) fn new(config: &Egress) -> Result<Self, String> {
        let mut roots = rustls::RootCertStore::empty();
        if config.system_roots {
            let found = rustls_native_certs::load_native_certs();
            for e in &found.errors {
                tracing::warn!(error = %e, "egress: a system root certificate could not be read");
            }
            let (added, skipped) = roots.add_parsable_certificates(found.certs);
            if skipped > 0 {
                tracing::warn!(added, skipped, "egress: system root certificates skipped");
            }
        }
        if let Some(path) = &config.ca_file {
            let fail =
                |e: &dyn std::fmt::Display| format!("egress CA file {}: {e}", path.display());
            let mut any = false;
            for cert in CertificateDer::pem_file_iter(path).map_err(|e| fail(&e))? {
                roots
                    .add(cert.map_err(|e| fail(&e))?)
                    .map_err(|e| fail(&e))?;
                any = true;
            }
            if !any {
                return Err(fail(&"no certificates"));
            }
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("egress TLS: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let mut http = HttpConnector::new_with_resolver(Guard {
            inner: GaiResolver::new(),
            allow_loopback: config.allow_loopback,
        });
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(config.connect_timeout));
        let https = HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(config.pool_idle_timeout)
            .pool_max_idle_per_host(config.pool_max_idle_per_host)
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .build(https);
        Ok(Self { client })
    }

    /// Send `request` and return its response once the headers arrive,
    /// within `first_byte`; the body then fails if a chunk takes longer
    /// than `between` to come. `permit` is held until the body ends.
    pub(crate) fn send(
        &self,
        request: hyper::Request<WasiBody>,
        first_byte: Duration,
        between: Duration,
        permit: OwnedSemaphorePermit,
    ) -> impl Future<Output = Result<hyper::Response<WasiBody>, Error>> + Send + 'static {
        let sent = self.client.request(request);
        async move {
            let response = tokio::time::timeout(first_byte, sent)
                .await
                .map_err(|_| Error::ConnectionReadTimeout)?
                .map_err(|e| classify(&e))?;
            Ok(response.map(|body| {
                Timed {
                    inner: body,
                    between,
                    sleep: Box::pin(tokio::time::sleep(between)),
                    _permit: permit,
                }
                .boxed_unsync()
            }))
        }
    }
}

/// The wasi:http error for a client error, from what caused it.
fn classify(e: &hyper_util::client::legacy::Error) -> Error {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(e);
    let mut dns = false;
    while let Some(c) = cause {
        // `io::Error::source` skips the error it wraps (often another
        // `io::Error`), so look inside each.
        let mut wrapped = vec![c];
        while let Some(inner) = wrapped
            .last()
            .and_then(|w| w.downcast_ref::<std::io::Error>())
            .and_then(|io| io.get_ref())
        {
            wrapped.push(inner);
        }
        for err in wrapped {
            if err.downcast_ref::<Prohibited>().is_some() {
                return Error::DestinationIpProhibited;
            }
            if let Some(tls) = err.downcast_ref::<rustls::Error>() {
                return match tls {
                    rustls::Error::InvalidCertificate(_) => Error::TlsCertificateError,
                    rustls::Error::AlertReceived(_) => Error::TlsAlertReceived {
                        alert_id: None,
                        alert_message: Some(tls.to_string()),
                    },
                    _ => Error::TlsProtocolError,
                };
            }
            if let Some(io) = err.downcast_ref::<std::io::Error>() {
                match io.kind() {
                    std::io::ErrorKind::ConnectionRefused => return Error::ConnectionRefused,
                    std::io::ErrorKind::TimedOut => return Error::ConnectionTimeout,
                    std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof => return Error::ConnectionTerminated,
                    _ => {}
                }
            }
            if err.to_string() == "dns error" {
                dns = true;
            }
        }
        cause = c.source();
    }
    if dns {
        return Error::DnsError {
            rcode: None,
            info_code: None,
        };
    }
    if e.is_connect() {
        Error::DestinationUnavailable
    } else {
        Error::HttpProtocolError
    }
}

/// A response body that fails when a chunk is late, and holds the app's
/// request permit until it ends.
struct Timed {
    inner: Incoming,
    between: Duration,
    sleep: Pin<Box<Sleep>>,
    _permit: OwnedSemaphorePermit,
}

impl Body for Timed {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                let next = tokio::time::Instant::now() + this.between;
                this.sleep.as_mut().reset(next);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(Error::from(e)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Some(Err(Error::ConnectionReadTimeout))),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// A name that resolved only to addresses egress refuses.
#[derive(Debug, thiserror::Error)]
#[error("{0} resolves only to loopback or link-local addresses")]
struct Prohibited(String);

/// The system resolver, without the addresses egress refuses.
#[derive(Clone)]
struct Guard {
    inner: GaiResolver,
    allow_loopback: bool,
}

impl tower_service::Service<Name> for Guard {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, std::io::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let allow_loopback = self.allow_loopback;
        let text = name.as_str().to_string();
        let lookup = self.inner.call(name);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = lookup
                .await?
                .filter(|a| allow_loopback || !guarded(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(std::io::Error::other(Prohibited(text)));
            }
            Ok(addrs.into_iter())
        })
    }
}
