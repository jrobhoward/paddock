//! HTTP handlers (ROADMAP E6): an unmodified `wasi:http` component gets a
//! new instance per request on its routes, with streaming bodies; the
//! `paddock-fixture-http` guest.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::fixture;
use paddock_host::http::{HttpIngress, HttpRoute, IngressListen, LoadedHandler};
use paddock_host::{InstanceSpec, Runtime, RuntimeConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The fixture built with `v2` (`/hello` answers `hello v2`), in a target
/// directory of its own.
fn fixture_v2() -> Vec<u8> {
    static BUILT: OnceLock<()> = OnceLock::new();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target_dir = root.join("target/fixtures/http-v2");
    BUILT.get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "-p",
                "paddock-fixture-http",
                "--features",
                "v2",
                "--target-dir",
            ])
            .arg(&target_dir)
            .status()
            .expect("run cargo to build the HTTP fixture v2");
        assert!(status.success(), "building the HTTP fixture v2 failed");
    });
    let path = target_dir.join("wasm32-wasip2/release/paddock_fixture_http.wasm");
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn handler_spec(app: &str) -> InstanceSpec {
    InstanceSpec {
        memory_max: 32 << 20,
        callback_deadline: Duration::from_millis(300),
        ..InstanceSpec::new(app, "1.0.0")
    }
}

/// A route on `prefix` with a 1,000-byte body cap and a 400 ms timeout.
fn route(prefix: &str) -> HttpRoute {
    HttpRoute {
        host: None,
        path_prefix: prefix.into(),
        hash_header: None,
        max_body: 1_000,
        timeout: Duration::from_millis(400),
    }
}

struct Served {
    _tokio: tokio::runtime::Runtime,
    rt: Runtime,
    ingress: HttpIngress,
    address: SocketAddr,
    v1: LoadedHandler,
    v2: LoadedHandler,
}

/// One runtime and ingress shared by every test: the `web` handler on
/// `/` and a chaos instance registered as `pong`.
fn served() -> &'static Served {
    static SERVED: OnceLock<Served> = OnceLock::new();
    SERVED.get_or_init(|| std::thread::spawn(start).join().unwrap())
}

fn start() -> Served {
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 32,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let v1 = rt
        .load_handler(&fixture("paddock_fixture_http"), None)
        .unwrap();
    let v2 = rt.load_handler(&fixture_v2(), None).unwrap();
    let chaos = rt.load(&fixture("paddock_fixture_chaos")).unwrap();
    let (ingress, address) = tokio.block_on(async {
        let apps = rt.start_apps(vec![]).await.unwrap();
        let ingress = HttpIngress::new(apps);
        ingress.set_routes("web", vec![route("/")]).unwrap();
        ingress.set_handler("web", v1.clone(), handler_spec("web"));
        let mut pong = common::spec("register:pong");
        pong.registered_names = vec!["pong".into()];
        rt.spawn(&chaos, pong).unwrap();
        while rt.whereis("pong").is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let any = "127.0.0.1:0".parse().unwrap();
        let address = ingress.listen(IngressListen::new(any)).await.unwrap();
        (ingress, address)
    });
    Served {
        _tokio: tokio,
        rt,
        ingress,
        address,
        v1,
        v2,
    }
}

#[derive(Debug)]
struct Got {
    /// 0 if the connection closed before a status line.
    status: u16,
    head: String,
    body: String,
    /// The body ended as HTTP/1.1 says it should (not cut).
    complete: bool,
}

/// Send `raw` on a new connection and read the response until the server
/// closes it.
async fn send(address: SocketAddr, raw: &[u8]) -> Got {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(raw).await.unwrap();
    let mut bytes = Vec::new();
    // A cut stream may end in a reset; keep what arrived.
    let _ = stream.read_to_end(&mut bytes).await;
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let Some((head, rest)) = raw.split_once("\r\n\r\n") else {
        return Got {
            status: 0,
            head: raw,
            body: String::new(),
            complete: false,
        };
    };
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let head = head.to_ascii_lowercase();
    let (body, complete) = if head.contains("transfer-encoding: chunked") {
        dechunk(rest)
    } else {
        let length = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length: "))
            .and_then(|n| n.trim().parse::<usize>().ok());
        (rest.to_string(), length.is_none_or(|n| n == rest.len()))
    };
    Got {
        status,
        head,
        body,
        complete,
    }
}

/// A chunked body's data, and whether its last (empty) chunk arrived.
fn dechunk(mut rest: &str) -> (String, bool) {
    let mut body = String::new();
    loop {
        let Some((size, after)) = rest.split_once("\r\n") else {
            return (body, false);
        };
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else {
            return (body, false);
        };
        if size == 0 {
            return (body, true);
        }
        if after.len() < size {
            body.push_str(after);
            return (body, false);
        }
        body.push_str(&after[..size]);
        rest = after[size..].trim_start_matches("\r\n");
    }
}

async fn get_at(address: SocketAddr, path: &str) -> Got {
    let raw = format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n");
    send(address, raw.as_bytes()).await
}

async fn get(path: &str) -> Got {
    get_at(served().address, path).await
}

async fn post(path: &str, body: &str) -> Got {
    let raw = format!(
        "POST {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    send(served().address, raw.as_bytes()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request____on_a_handler_route____answered_by_a_new_instance() {
    let first = get("/self").await;

    let second = get("/self").await;
    let hello = get("/hello").await;

    assert_eq!((first.status, second.status), (200, 200));
    assert_ne!(first.body, second.body, "each request has its own instance");
    assert_eq!((hello.status, hello.body.as_str()), (200, "hello"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn echo____body_under_max_body____streamed_back() {
    let body = "x".repeat(900);

    let got = post("/echo", &body).await;

    assert_eq!(got.status, 200);
    assert!(got.complete, "{got:?}");
    assert_eq!(got.body, body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_body____declared_over_it____413() {
    let got = post("/echo", &"x".repeat(1_001)).await;

    assert_eq!(got.status, 413);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_body____chunked_body_over_it____guest_sees_an_error_and_echo_is_cut() {
    let chunk = "y".repeat(600);
    let raw = format!(
        "POST /echo HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ntransfer-encoding: chunked\r\n\r\n\
         258\r\n{chunk}\r\n258\r\n{chunk}\r\n0\r\n\r\n"
    );

    let got = send(served().address, raw.as_bytes()).await;

    assert_eq!(got.status, 200);
    assert!(!got.complete, "{got:?}");
    assert!(got.body.len() <= 1_000, "{} bytes echoed", got.body.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout____body_keeps_moving_past_it____response_completes() {
    let started = Instant::now();

    // Six chunks 150 ms apart: 900 ms in all, against a 400 ms timeout.
    let got = get("/drip?chunks=6&ms=150").await;

    assert_eq!(got.status, 200);
    assert!(got.complete, "{got:?}");
    assert_eq!(got.body, "drip\n".repeat(6));
    assert!(started.elapsed() >= Duration::from_millis(800));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout____body_stalls_longer_than_it____stream_cut() {
    let got = get("/drip?chunks=3&ms=700").await;

    assert_eq!(got.status, 200);
    assert!(!got.complete, "{got:?}");
}

#[rstest::rstest]
#[case::no_headers_in_time("/hang", 504)]
#[case::spins_past_callback_deadline("/spin", 504)]
#[case::traps("/trap", 500)]
#[case::sets_no_response("/none", 500)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure____before_headers____mapped_to_status_and_counted(
    #[case] path: &str,
    #[case] status: u16,
) {
    let before = served().ingress.stats().handler_failures;
    let started = Instant::now();

    let got = get(path).await;

    assert_eq!(got.status, status, "{got:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(served().ingress.stats().handler_failures > before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trap____after_setting_the_response____stream_cut() {
    let got = get("/trap-late").await;

    // The connection closes without the last chunk; whether the headers
    // and the first chunk went out first depends on when hyper flushed.
    assert!(!got.complete, "{got:?}");
    assert!(got.status == 0 || got.status == 200, "{got:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process____call_to_a_registered_app____its_reply() {
    let got = get("/call?name=pong&body=echo:hi").await;

    assert_eq!((got.status, got.body.as_str()), (200, "hi"));
}

/// Give app `app` the host `<app>.example` (any path) and the v1 handler:
/// an app of its own for a test that changes it.
fn own_app(app: &str) {
    let s = served();
    let route = HttpRoute {
        host: Some(format!("{app}.example")),
        ..route("/")
    };
    s.ingress.set_routes(app, vec![route]).unwrap();
    s.ingress.set_handler(app, s.v1.clone(), handler_spec(app));
}

async fn get_host(app: &str, path: &str) -> Got {
    let raw = format!("GET {path} HTTP/1.1\r\nhost: {app}.example\r\nconnection: close\r\n\r\n");
    send(served().address, raw.as_bytes()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handler_requests____during_a_request____counted_then_released() {
    let s = served();
    own_app("count");
    let slow = tokio::spawn(get_host("count", "/drip?chunks=4&ms=100"));
    tokio::time::sleep(Duration::from_millis(150)).await;

    let during = s.ingress.handler_requests("count");
    slow.await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while s.ingress.handler_requests("count") != Some(0) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(during, Some(1));
    assert_eq!(s.ingress.handler_requests("count"), Some(0));
    assert_eq!(s.ingress.handler_requests("nobody"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_handler____upgrade_with_a_request_in_flight____new_requests_get_it_old_one_finishes() {
    let s = served();
    own_app("up");
    let in_flight = tokio::spawn(get_host("up", "/drip?chunks=4&ms=100"));
    tokio::time::sleep(Duration::from_millis(50)).await;

    s.ingress
        .set_handler("up", s.v2.clone(), handler_spec("up"));
    let after = get_host("up", "/hello").await;
    let old = in_flight.await.unwrap();

    assert_eq!(after.body, "hello v2");
    assert!(old.complete, "{old:?}");
    assert_eq!(old.body, "drip\n".repeat(4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_handler____then_request____503_app_not_running() {
    let s = served();
    own_app("gone");
    let before = get_host("gone", "/hello").await;

    s.ingress.remove_handler("gone");
    let after = get_host("gone", "/hello").await;

    assert_eq!(before.status, 200);
    assert_eq!(after.status, 503);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_slots____all_in_use____503_with_retry_after() {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 1,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let handler = rt
        .load_handler(&fixture("paddock_fixture_http"), None)
        .unwrap();
    let ingress = HttpIngress::new(rt.start_apps(vec![]).await.unwrap());
    ingress.set_routes("web", vec![route("/")]).unwrap();
    ingress.set_handler("web", handler, handler_spec("web"));
    let address = ingress
        .listen(IngressListen::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let holding = tokio::spawn(get_at(address, "/drip?chunks=3&ms=100"));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let refused = get_at(address, "/hello").await;
    let held = holding.await.unwrap();
    ingress.stop();

    assert_eq!(refused.status, 503);
    assert!(refused.head.contains("retry-after: 1"), "{}", refused.head);
    assert_eq!(held.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_handler____app_component____refused() {
    let s = served();

    let result = s.rt.load_handler(&fixture("paddock_fixture_echo"), None);

    assert!(result.is_err());
}
