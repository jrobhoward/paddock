//! HTTP ingress throughput, for `docs/BASELINE.md` (ROADMAP B7): the
//! node's ingress (`HttpIngress`, hyper in the host) on loopback, each
//! request a call to a pool of the SDK fixture answering `/plain` with
//! `plain`; or, with `handler`, a new instance of the HTTP-handler fixture
//! per request answering `/hello` with `hello`, streamed (ROADMAP E6); or,
//! with `s15`, spike S15's host-hyper shape (a bare hyper service calling
//! chaos instances round-robin with `echo:ok`, no envelopes, no routing),
//! so the ingress's own cost shows against it under the same client (ROADMAP
//! B8); or, with `chaos`, the ingress in front of a pool of chaos instances,
//! which answer any request with a short text reply (no CBOR in the guest):
//! the ingress's host cost alone. The load comes from keep-alive HTTP/1.1 connections on their own
//! tokio runtime, as in spike S15.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-sdk -p paddock-fixture-http
//! cargo run --release -p paddock-host --example ingress_throughput -- 4 4
//! cargo run --release -p paddock-host --example ingress_throughput -- 4 4 handler
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos --target-dir target/fixtures
//! cargo run --release -p paddock-host --example ingress_throughput -- 4 4 s15
//! ```
//!
//! Arguments: server tokio workers (default 4), pool instances (default
//! 4; unused with `handler`), and `handler`, `s15` or `chaos`. The client runtime
//! has 4 workers.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use paddock_bundle::manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};
use paddock_host::http::{HttpIngress, HttpRoute, IngressListen};
use paddock_host::{AppDeployment, InstanceSpec, Runtime, RuntimeConfig};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMED: Duration = Duration::from_secs(5);

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |a| a.parse().expect("workers"));
    let pool: usize = std::env::args()
        .nth(2)
        .map_or(4, |a| a.parse().expect("instances"));
    let mode = std::env::args().nth(3).unwrap_or_default();
    let (handler, s15) = (mode == "handler", mode == "s15");
    let path = match mode.as_str() {
        "handler" => "target/wasm32-wasip2/release/paddock_fixture_http.wasm",
        "s15" | "chaos" => "target/fixtures/wasm32-wasip2/release/paddock_fixture_chaos.wasm",
        _ => "target/wasm32-wasip2/release/paddock_fixture_sdk.wasm",
    };
    let path = format!("{}/../../{path}", env!("CARGO_MANIFEST_DIR"));
    let component = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let server = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name("server")
        .enable_all()
        .build()
        .unwrap();
    let (address, _rt, _ingress) = if s15 {
        let (address, rt) = server.block_on(serve_s15(component, pool));
        (address, rt, None)
    } else {
        let (address, rt, ingress) = server.block_on(serve(component, pool, handler));
        (address, rt, Some(ingress))
    };
    let request: &'static [u8] = if handler || s15 {
        b"GET /hello HTTP/1.1\r\nhost: localhost\r\n\r\n"
    } else {
        b"GET /plain HTTP/1.1\r\nhost: localhost\r\n\r\n"
    };
    let client = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("client")
        .enable_all()
        .build()
        .unwrap();
    if s15 {
        println!(
            "ingress_throughput: {workers} server workers, S15's shape (bare hyper, {pool} chaos instances), 4 client workers"
        );
    } else if handler {
        println!(
            "ingress_throughput: {workers} server workers, an HTTP handler (an instance per request), 4 client workers"
        );
    } else {
        println!(
            "ingress_throughput: {workers} server workers, a pool of {pool}, 4 client workers"
        );
    }
    client.block_on(async move {
        let mut us = Vec::new();
        let mut conn = Conn::open(address, request).await;
        for _ in 0..1_000 {
            conn.get().await;
        }
        let started = Instant::now();
        while started.elapsed() < TIMED {
            let t = Instant::now();
            conn.get().await;
            us.push(t.elapsed().as_secs_f64() * 1e6);
        }
        us.sort_by(f64::total_cmp);
        println!(
            "1 connection: {:.0} requests/s, p50 {:.0} us, p99 {:.0} us",
            us.len() as f64 / TIMED.as_secs_f64(),
            us[us.len() / 2],
            us[(us.len() - 1) * 99 / 100]
        );
        for connections in [8usize, 32, 128] {
            let mut tasks = Vec::new();
            let deadline = Instant::now() + TIMED;
            for _ in 0..connections {
                tasks.push(tokio::spawn(async move {
                    let mut conn = Conn::open(address, request).await;
                    let mut n = 0u64;
                    while Instant::now() < deadline {
                        conn.get().await;
                        n += 1;
                    }
                    n
                }));
            }
            let mut total = 0;
            for t in tasks {
                total += t.await.unwrap();
            }
            println!(
                "{connections} connections: {:.0} requests/s",
                total as f64 / TIMED.as_secs_f64()
            );
        }
    });
}

async fn serve(
    component: Vec<u8>,
    pool: usize,
    handler: bool,
) -> (SocketAddr, Runtime, HttpIngress) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 256,
        ..RuntimeConfig::default()
    })
    .unwrap();
    if handler {
        let loaded = rt.load_handler(&component, None).unwrap();
        let ingress = HttpIngress::new(rt.start_apps(vec![]).await.unwrap());
        let route = HttpRoute {
            host: None,
            path_prefix: "/hello".into(),
            hash_header: None,
            max_body: 1 << 20,
            timeout: Duration::from_secs(30),
        };
        ingress.set_routes("web", vec![route]).unwrap();
        ingress.set_handler("web", loaded, InstanceSpec::new("web", "1.0.0"));
        let address = ingress
            .listen(IngressListen::new("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        return (address, rt, ingress);
    }
    let app = rt.load(&component).unwrap();
    let manifest = AppManifest {
        app: AppInfo {
            name: "web".into(),
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
        spec: AppSpec::from_json_value(&json!({
            "instances": { "count": pool },
            "capabilities": { "wasi:http/incoming-handler": {
                "routes": [{ "path_prefix": "/plain" }]
            } }
        }))
        .unwrap(),
    };
    let routes = HttpRoute::from_manifest(&manifest);
    let apps = rt
        .start_apps(vec![AppDeployment::new(manifest, app)])
        .await
        .unwrap();
    let ingress = HttpIngress::new(apps);
    ingress.set_routes("web", routes).unwrap();
    let address = ingress
        .listen(IngressListen::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    (address, rt, ingress)
}

/// Spike S15's host-hyper shape: a bare hyper service, each request a call
/// to one of `pool` chaos instances in turn, answering its reply's body.
async fn serve_s15(component: Vec<u8>, pool: usize) -> (SocketAddr, Runtime) {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use http_body_util::Full;
    use hyper_util::rt::TokioIo;
    use paddock_host::Envelope;

    let rt = Runtime::new(RuntimeConfig {
        max_instances: 64,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    let instances: Arc<Vec<_>> = Arc::new(
        (0..pool)
            .map(|_| rt.spawn(&app, InstanceSpec::new("chaos", "0.1.0")).unwrap())
            .collect(),
    );
    let next = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            stream.set_nodelay(true).unwrap();
            let (instances, next) = (instances.clone(), next.clone());
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |_req| {
                    let (instances, next) = (instances.clone(), next.clone());
                    async move {
                        let i = next.fetch_add(1, Ordering::Relaxed) % instances.len();
                        let reply = instances[i]
                            .call(
                                Envelope::new("text/plain", "echo:ok"),
                                Duration::from_secs(5),
                            )
                            .await
                            .unwrap();
                        Ok::<_, Infallible>(hyper::Response::new(Full::new(Bytes::from(
                            reply.body,
                        ))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (address, rt)
}

/// One keep-alive connection sending `request` again and again.
struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
    request: &'static [u8],
}

impl Conn {
    async fn open(address: SocketAddr, request: &'static [u8]) -> Self {
        let stream = TcpStream::connect(address).await.unwrap();
        stream.set_nodelay(true).unwrap();
        Self {
            stream,
            buf: Vec::with_capacity(4096),
            request,
        }
    }

    /// One request; reads the headers, then `content-length` bytes or, for
    /// a chunked body, up to its last chunk.
    async fn get(&mut self) {
        self.stream.write_all(self.request).await.unwrap();
        loop {
            if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buf[..end]).to_ascii_lowercase();
                assert!(head.starts_with("http/1.1 200"), "{head}");
                if head.contains("transfer-encoding: chunked") {
                    // The fixtures' bodies never contain the last chunk's bytes.
                    let last = loop {
                        if let Some(at) = self.buf[end..].windows(5).position(|w| w == b"0\r\n\r\n")
                        {
                            break end + at + 5;
                        }
                        self.read().await;
                    };
                    self.buf.drain(..last);
                    return;
                }
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().unwrap());
                let total = end + 4 + len;
                while self.buf.len() < total {
                    self.read().await;
                }
                self.buf.drain(..total);
                return;
            }
            self.read().await;
        }
    }

    async fn read(&mut self) {
        let mut chunk = [0u8; 4096];
        let n = self.stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "connection closed");
        self.buf.extend_from_slice(&chunk[..n]);
    }
}
