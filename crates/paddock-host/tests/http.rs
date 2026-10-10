//! HTTP ingress (ROADMAP E1): requests to an app's routes become calls to
//! its pool, through `paddock-sdk`'s `http` module and the SDK fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::sync::OnceLock;

use common::fixture;
use paddock_bundle::manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};
use paddock_host::http::{HttpIngress, HttpRoute};
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
    let (apps, ingress, address) = tokio.block_on(async {
        let apps = rt
            .start_apps(vec![AppDeployment::new(web, app)])
            .await
            .unwrap();
        let ingress = HttpIngress::new(apps.clone());
        ingress.set_routes("web", routes).unwrap();
        ingress.set_routes("ghost", vec![route("/ghost")]).unwrap();
        let address = ingress
            .listen("127.0.0.1:0".parse().unwrap(), 2)
            .await
            .unwrap();
        (apps, ingress, address)
    });
    Served {
        _tokio: tokio,
        _rt: rt,
        _apps: apps,
        ingress,
        address,
    }
}

fn route(prefix: &str) -> HttpRoute {
    HttpRoute {
        host: None,
        path_prefix: prefix.into(),
        hash_header: None,
        max_body: 1024,
        timeout: std::time::Duration::from_secs(1),
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
            .starts_with("POST /api/orders?y=1 key=t1 body=hi pid="),
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
