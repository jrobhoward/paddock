//! HTTP ingress (ROADMAP E1): requests to an app's routes become calls to
//! its pool, through `paddock-sdk`'s `http` module and the SDK fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use common::fixture;
use paddock_bundle::manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};
use paddock_host::http::{HttpIngress, HttpRoute, IngressListen, IngressTls, TlsCertificate};
use paddock_host::{AppDeployment, Apps, Runtime, RuntimeConfig};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn manifest(name: &str, appserver: serde_json::Value) -> AppManifest {
    AppManifest {
        app: AppInfo {
            name: name.into(),
            version: "1.0.0".parse().unwrap(),
            description: None,
            authors: vec![],
            license: None,
            repository: None,
        },
        component: ComponentRef {
            path: String::new(),
            checksum: String::new(),
            world: "paddock:runtime/app@0.1.0".into(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(&appserver).unwrap(),
    }
}

struct Served {
    /// The ingress and apps run here, outliving every test's own runtime.
    _tokio: tokio::runtime::Runtime,
    _rt: Runtime,
    _apps: Apps,
    ingress: HttpIngress,
    address: SocketAddr,
    /// 300 ms for headers and body, 1,000-byte replies.
    tight: SocketAddr,
    /// One connection at a time.
    capped: SocketAddr,
    /// HTTPS with certificates for `web.example` (the default) and
    /// `*.shop.example`.
    tls: SocketAddr,
    /// The certificates' DER, to trust and to compare with what is served.
    web_cert: Vec<u8>,
    shop_cert: Vec<u8>,
    _certs: tempfile::TempDir,
}

/// One runtime, the `web` app (a pool of three routed by `key`) and an
/// ingress on a free port, shared by every test.
fn served() -> &'static Served {
    static SERVED: OnceLock<Served> = OnceLock::new();
    SERVED.get_or_init(|| {
        // Built on its own thread: a test's runtime cannot block on another.
        std::thread::spawn(start).join().unwrap()
    })
}

fn start() -> Served {
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&fixture("paddock_fixture_sdk")).unwrap();
    let web = manifest(
        "web",
        json!({
            "instances": { "count": 3, "routing": "hash:key" },
            "supervision": { "intensity": 100, "backoff": { "initial_ms": 1, "max_ms": 5 } },
            "limits": { "callback_deadline_ms": 2000 },
            "capabilities": { "wasi:http/incoming-handler": {
                "routes": [
                    { "path_prefix": "/api", "hash_header": "x-tenant" },
                    { "host": "shop.example", "path_prefix": "/shop" }
                ],
                "max_body": 64,
                "timeout_ms": 300
            } }
        }),
    );
    let routes = HttpRoute::from_manifest(&web);
    let certs = tempfile::tempdir().unwrap();
    let (web_cert, web_entry) = cert(certs.path(), "web", &["web.example"]);
    let (shop_cert, shop_entry) = cert(certs.path(), "shop", &["*.shop.example"]);
    let tls_certs = IngressTls::load(vec![web_entry, shop_entry]).unwrap();
    let (apps, ingress, address, tight, capped, tls) = tokio.block_on(async {
        let apps = rt
            .start_apps(vec![AppDeployment::new(web, app)])
            .await
            .unwrap();
        let ingress = HttpIngress::new(apps.clone());
        ingress.set_routes("web", routes).unwrap();
        ingress.set_routes("ghost", vec![route("/ghost")]).unwrap();
        let any = "127.0.0.1:0".parse().unwrap();
        let address = ingress.listen(IngressListen::new(any)).await.unwrap();
        let tight = ingress
            .listen(IngressListen {
                header_timeout: Duration::from_millis(300),
                body_timeout: Duration::from_millis(300),
                max_response: 1_000,
                ..IngressListen::new(any)
            })
            .await
            .unwrap();
        let capped = ingress
            .listen(IngressListen {
                max_connections: 1,
                ..IngressListen::new(any)
            })
            .await
            .unwrap();
        let tls = ingress
            .listen(IngressListen {
                tls: Some(tls_certs),
                ..IngressListen::new(any)
            })
            .await
            .unwrap();
        (apps, ingress, address, tight, capped, tls)
    });
    Served {
        _tokio: tokio,
        _rt: rt,
        _apps: apps,
        ingress,
        address,
        tight,
        capped,
        tls,
        web_cert,
        shop_cert,
        _certs: certs,
    }
}

/// A self-signed certificate for `hosts`, written to `dir`; its DER and the
/// ingress entry for it.
fn cert(dir: &std::path::Path, name: &str, hosts: &[&str]) -> (Vec<u8>, TlsCertificate) {
    let made = rcgen::generate_simple_self_signed(
        hosts.iter().map(ToString::to_string).collect::<Vec<_>>(),
    )
    .unwrap();
    let cert = dir.join(format!("{name}.pem"));
    let key = dir.join(format!("{name}.key"));
    std::fs::write(&cert, made.cert.pem()).unwrap();
    std::fs::write(&key, made.signing_key.serialize_pem()).unwrap();
    let entry = TlsCertificate {
        cert,
        key,
        hosts: hosts.iter().map(ToString::to_string).collect(),
    };
    (made.cert.der().to_vec(), entry)
}

fn route(prefix: &str) -> HttpRoute {
    HttpRoute {
        host: None,
        path_prefix: prefix.into(),
        hash_header: None,
        max_body: 1024,
        timeout: Duration::from_secs(1),
    }
}

struct Got {
    status: u16,
    head: String,
    body: String,
}

/// One HTTP/1.1 request on its own connection.
async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Got {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req.push_str("host: localhost\r\n");
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Got {
        status,
        head: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

async fn get(path: &str, headers: &[(&str, &str)]) -> Got {
    request(served().address, "GET", path, headers, "").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request____on_a_route____answered_by_the_app() {
    let s = served();

    let got = request(
        s.address,
        "POST",
        "/api/orders?y=1",
        &[("x-tenant", "t1")],
        "hi",
    )
    .await;

    assert_eq!(got.status, 200);
    assert!(
        got.body
            .starts_with("POST /api/orders?y=1 key=t1 body=hi scheme=http pid="),
        "{}",
        got.body
    );
    assert!(got.head.contains("x-fixture: sdk"), "{}", got.head);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hash_header____same_tenant____same_instance() {
    let pid = |g: Got| g.body.rsplit("pid=").next().unwrap().to_string();

    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(pid(get("/api/x", &[("x-tenant", "acme")]).await));
    }

    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routes____host_and_prefix____matched_on_segments() {
    let named = get("/shop/cart", &[("host", "Shop.Example:8080")]).await;
    let other_host = get("/shop/cart", &[("host", "elsewhere")]).await;
    let not_segment = get("/apix", &[]).await;

    assert_eq!(named.status, 200);
    assert_eq!(other_host.status, 404);
    assert_eq!(not_segment.status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies____plain_status_and_failures____mapped_to_http() {
    let s = served();

    let plain = get("/api/plain", &[]).await;
    let teapot = get("/api/status/418", &[]).await;
    let never = get("/api/never", &[]).await;
    let crash = get("/api/crash", &[]).await;
    let big = request(s.address, "POST", "/api/x", &[], &"x".repeat(65)).await;
    let ghost = get("/ghost", &[]).await;

    assert_eq!((plain.status, plain.body.as_str()), (200, "plain"));
    assert!(
        plain.head.contains("content-type: text/plain"),
        "{}",
        plain.head
    );
    assert_eq!(teapot.status, 418);
    assert_eq!(never.status, 504);
    assert_eq!(crash.status, 502);
    assert_eq!(big.status, 413);
    assert_eq!(ghost.status, 503);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_routes____another_apps_route____refused() {
    let s = served();

    let clash = s.ingress.set_routes("thief", vec![route("/api")]);
    let own_again = s.ingress.set_routes("ghost", vec![route("/ghost")]);

    assert!(clash.unwrap_err().to_string().contains("belongs to web"));
    assert!(own_again.is_ok());
}

/// Read until the server closes or `limit` passes; what arrived.
async fn read_until_closed(stream: &mut tokio::net::TcpStream, limit: Duration) -> Option<String> {
    let mut raw = Vec::new();
    match tokio::time::timeout(limit, stream.read_to_end(&mut raw)).await {
        Ok(_) => Some(String::from_utf8_lossy(&raw).into_owned()),
        Err(_) => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_client____headers_or_body_late____dropped_or_408() {
    let s = served();
    let mut silent = tokio::net::TcpStream::connect(s.tight).await.unwrap();
    let mut slow_body = tokio::net::TcpStream::connect(s.tight).await.unwrap();
    slow_body
        .write_all(b"POST /api/x HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10\r\n\r\nab")
        .await
        .unwrap();
    let started = Instant::now();

    let silent_end = read_until_closed(&mut silent, Duration::from_secs(5)).await;
    let body_end = read_until_closed(&mut slow_body, Duration::from_secs(5)).await;

    assert!(silent_end.is_some(), "an idle connection stayed open");
    assert!(started.elapsed() < Duration::from_secs(5));
    let body_end = body_end.expect("a slow body held the connection");
    assert!(body_end.starts_with("HTTP/1.1 408"), "{body_end}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_response____reply_over_it____502() {
    let s = served();

    let small = request(s.tight, "GET", "/api/big/10", &[], "").await;
    let big = request(s.tight, "GET", "/api/big/2000", &[], "").await;

    assert_eq!((small.status, small.body.len()), (200, 10));
    assert_eq!(big.status, 502);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_connections____over_it____closed_on_accept() {
    let s = served();
    let refused_before = s.ingress.stats().refused;
    let _held = tokio::net::TcpStream::connect(s.capped).await.unwrap();
    // Let the accept task take the first connection.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut second = tokio::net::TcpStream::connect(s.capped).await.unwrap();

    let ended = read_until_closed(&mut second, Duration::from_secs(2)).await;

    assert_eq!(ended.as_deref(), Some(""));
    assert!(s.ingress.stats().refused > refused_before);
}

/// One HTTPS request with SNI `server_name`, trusting both test
/// certificates: (the certificate served, status line, body).
async fn https(server_name: &str, path: &str) -> (Vec<u8>, String, String) {
    let s = served();
    let mut roots = rustls::RootCertStore::empty();
    for der in [&s.web_cert, &s.shop_cert] {
        roots
            .add(rustls_pki_types::CertificateDer::from(der.clone()))
            .unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(s.tls).await.unwrap();
    let name = rustls_pki_types::ServerName::try_from(server_name.to_string()).unwrap();
    let mut stream = connector.connect(name, tcp).await.unwrap();
    let served_cert = stream.get_ref().1.peer_certificates().unwrap()[0].to_vec();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: {server_name}\r\nconnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (
        served_cert,
        head.lines().next().unwrap().to_string(),
        body.to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls____sni_names____matching_certificate_and_https_scheme() {
    let s = served();

    let (web, status, body) = https("web.example", "/api/x").await;
    let (shop, _, _) = https("eu.shop.example", "/api/x").await;

    assert_eq!(web, s.web_cert);
    assert_eq!(shop, s.shop_cert);
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(body.contains("scheme=https"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls____plain_http_on_the_tls_port____no_response() {
    let s = served();
    let mut stream = tokio::net::TcpStream::connect(s.tls).await.unwrap();
    stream
        .write_all(b"GET /api/x HTTP/1.1\r\nhost: web.example\r\n\r\n")
        .await
        .unwrap();

    let ended = read_until_closed(&mut stream, Duration::from_secs(5)).await;

    let ended = ended.expect("the connection stayed open");
    assert!(!ended.contains("HTTP/1.1 200"), "{ended}");
}
