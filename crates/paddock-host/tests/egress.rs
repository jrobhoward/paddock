//! HTTP egress (`wasi:http/outgoing-handler`) through the SDK fixture's
//! `fetch` command, against a local server: the allow list, methods, the
//! address guard, the per-app limit, timeouts, the pool and TLS.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use common::{ask, fixture};
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::{Request, Response};
use paddock_bundle::manifest::Capability;
use paddock_host::{Egress, InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

/// A body that sends one chunk and then nothing, ever.
struct Stall(bool);

impl Body for Stall {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        if this.0 {
            Poll::Pending
        } else {
            this.0 = true;
            Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"first")))))
        }
    }
}

type Reply = Response<http_body_util::combinators::BoxBody<Bytes, Infallible>>;

async fn answer(req: Request<Incoming>) -> Result<Reply, Infallible> {
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    let full = |s: String| Full::new(Bytes::from(s)).boxed();
    Ok(match path.as_str() {
        "/hello" => Response::new(full("hi".into())),
        "/echo" => {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            Response::new(full(format!("{method} {}", String::from_utf8_lossy(&body))))
        }
        "/slow" => {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            Response::new(full("late".into()))
        }
        "/stall" => Response::new(Stall(false).boxed()),
        _ => {
            let mut r = Response::new(full("missing".into()));
            *r.status_mut() = hyper::StatusCode::NOT_FOUND;
            r
        }
    })
}

/// A local HTTP server (HTTPS with `tls`); its address and how many
/// connections it accepted.
async fn server(tls: Option<Arc<rustls::ServerConfig>>) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::Relaxed);
            let tls = tls.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(answer);
                let http = hyper::server::conn::http1::Builder::new();
                match tls {
                    Some(config) => {
                        let acceptor = tokio_rustls::TlsAcceptor::from(config);
                        if let Ok(stream) = acceptor.accept(stream).await {
                            let io = hyper_util::rt::TokioIo::new(stream);
                            let _ = http.serve_connection(io, service).await;
                        }
                    }
                    None => {
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let _ = http.serve_connection(io, service).await;
                    }
                }
            });
        }
    });
    (address, accepted)
}

fn runtime(egress: Egress) -> (Runtime, LoadedApp) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        egress,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&fixture("paddock_fixture_sdk")).unwrap();
    (rt, app)
}

fn spec(app: &str, allow: &[String], granted: bool) -> InstanceSpec {
    let mut s = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        granted: if granted {
            [Capability::HttpOutgoing].into_iter().collect()
        } else {
            Default::default()
        },
        ..InstanceSpec::new(app, "1.0.0")
    };
    s.services.egress.allow = allow.to_vec();
    s
}

fn up(rt: &(Runtime, LoadedApp), spec: InstanceSpec) -> InstanceHandle {
    rt.0.spawn(&rt.1, spec).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____allowed_address____responses_over_one_pooled_connection() {
    let (address, accepted) = server(None).await;
    let rt = runtime(Egress::default());
    let h = up(&rt, spec("egress-ok", &[format!("http://{address}")], true));

    let first = ask(&h, &format!("fetch GET http://{address}/hello")).await;
    let second = ask(&h, &format!("fetch POST http://{address}/echo payload")).await;

    assert_eq!(first, "200 hi");
    assert_eq!(second, "200 POST payload");
    assert_eq!(accepted.load(Ordering::Relaxed), 1, "pooled");
    let metrics = rt.0.render_metrics();
    assert!(
        metrics.contains("paddock_egress_requests_total{app=\"egress-ok\",outcome=\"sent\"} 2"),
        "{metrics}"
    );
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____not_granted_other_port_or_method____denied() {
    let (address, accepted) = server(None).await;
    let rt = runtime(Egress::default());
    let allow = [format!("http://{address}")];
    let ungranted = up(&rt, spec("egress-ungranted", &allow, false));
    let other_port = up(
        &rt,
        spec(
            "egress-port",
            &[format!("http://127.0.0.1:{}", address.port() + 1)],
            true,
        ),
    );
    let mut get_only = spec("egress-methods", &allow, true);
    get_only.services.egress.methods = vec!["GET".into()];
    let get_only = up(&rt, get_only);

    let a = ask(&ungranted, &format!("fetch GET http://{address}/hello")).await;
    let b = ask(&other_port, &format!("fetch GET http://{address}/hello")).await;
    let c = ask(&get_only, &format!("fetch POST http://{address}/echo x")).await;
    let d = ask(&get_only, &format!("fetch GET http://{address}/hello")).await;

    assert_eq!(
        (a.as_str(), b.as_str(), c.as_str()),
        ("err:denied", "err:denied", "err:denied")
    );
    assert_eq!(d, "200 hi");
    assert_eq!(accepted.load(Ordering::Relaxed), 1, "only the GET went out");
    for h in [ungranted, other_port, get_only] {
        h.kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____name_resolving_to_loopback____prohibited_unless_node_allows() {
    let (address, _) = server(None).await;
    let url = format!("fetch GET http://localhost:{}/hello", address.port());
    let allow = [format!("http://localhost:{}", address.port())];
    let guarded = runtime(Egress::default());
    let mut open = Egress::default();
    open.allow_loopback = true;
    let open = runtime(open);
    let a = up(&guarded, spec("egress-guarded", &allow, true));
    let b = up(&open, spec("egress-open", &allow, true));

    let refused = ask(&a, &url).await;
    let allowed = ask(&b, &url).await;

    assert_eq!(refused, "err:prohibited");
    assert_eq!(allowed, "200 hi");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch____app_at_max_concurrent____limited_until_one_finishes() {
    let (address, _) = server(None).await;
    let rt = runtime(Egress::default());
    let allow = [format!("http://{address}")];
    let with_limit = |app| {
        let mut s = spec(app, &allow, true);
        s.services.egress.max_concurrent = Some(1);
        s
    };
    let a = up(&rt, with_limit("egress-limit"));
    let b = up(&rt, with_limit("egress-limit"));

    let slow = tokio::spawn({
        let a = a.clone();
        let url = format!("fetch GET http://{address}/slow");
        async move { ask(&a, &url).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let while_busy = ask(&b, &format!("fetch GET http://{address}/hello")).await;
    let late = slow.await.unwrap();
    let after = ask(&b, &format!("fetch GET http://{address}/hello")).await;

    assert_eq!(while_busy, "err:limited");
    assert_eq!(late, "200 late");
    assert_eq!(after, "200 hi");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____slow_headers_or_stalled_body____timeout_and_instance_lives() {
    let (address, _) = server(None).await;
    let mut egress = Egress::default();
    egress.first_byte_timeout = Duration::from_millis(300);
    egress.between_bytes_timeout = Duration::from_millis(300);
    let rt = runtime(egress);
    let h = up(
        &rt,
        spec("egress-timeout", &[format!("http://{address}")], true),
    );

    let slow = ask(&h, &format!("fetch GET http://{address}/slow")).await;
    let stalled = ask(&h, &format!("fetch GET http://{address}/stall")).await;
    let fine = ask(&h, &format!("fetch GET http://{address}/hello")).await;

    assert_eq!(slow, "err:timeout");
    assert_eq!(stalled, "err:timeout");
    assert_eq!(fine, "200 hi");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch____https____trusted_with_the_ca_file_only() {
    let dir = tempfile::tempdir().unwrap();
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();
    let ca_file = dir.path().join("ca.pem");
    std::fs::write(&ca_file, ca.pem()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone()],
        rustls_pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
    )
    .unwrap();
    let (address, _) = server(Some(Arc::new(tls))).await;
    let mut trusting = Egress::default();
    trusting.system_roots = false;
    trusting.ca_file = Some(ca_file);
    let mut system_only = Egress::default();
    system_only.system_roots = false;
    let trusting = runtime(trusting);
    let untrusting = runtime(system_only);
    let allow = [format!("https://{address}")];
    let a = up(&trusting, spec("egress-tls", &allow, true));
    let b = up(&untrusting, spec("egress-tls-no-ca", &allow, true));

    let good = ask(&a, &format!("fetch GET https://{address}/hello")).await;
    let bad = ask(&b, &format!("fetch GET https://{address}/hello")).await;

    assert_eq!(good, "200 hi");
    assert!(bad.starts_with("err:tls"), "{bad}");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch____instance_killed_mid_request____its_slot_freed() {
    let (address, _) = server(None).await;
    let rt = runtime(Egress::default());
    let allow = [format!("http://{address}")];
    let with_limit = |app| {
        let mut s = spec(app, &allow, true);
        s.services.egress.max_concurrent = Some(1);
        s
    };
    let a = up(&rt, with_limit("egress-killed"));
    let b = up(&rt, with_limit("egress-killed"));
    let slow = tokio::spawn({
        let a = a.clone();
        let url = format!("fetch GET http://{address}/slow");
        async move { ask(&a, &url).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let before = ask(&b, &format!("fetch GET http://{address}/hello")).await;
    a.kill();
    let _ = slow.await;
    let mut after = String::new();
    for _ in 0..50 {
        after = ask(&b, &format!("fetch GET http://{address}/hello")).await;
        if after != "err:limited" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(before, "err:limited");
    assert_eq!(after, "200 hi");
    b.kill();
}
