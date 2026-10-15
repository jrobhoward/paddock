//! HTTP ingress throughput, for `docs/BASELINE.md` (ROADMAP B7): the
//! node's ingress (`HttpIngress`, hyper in the host) on loopback, each
//! request a call to a pool of the SDK fixture answering `/plain` with
//! `plain`. The load comes from keep-alive HTTP/1.1 connections on their own
//! tokio runtime, as in spike S15.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-sdk
//! cargo run --release -p paddock-host --example ingress_throughput -- 4 4
//! ```
//!
//! Arguments: server tokio workers (default 4) and pool instances (default
//! 4). The client runtime has 4 workers.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use paddock_bundle::manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};
use paddock_host::http::{HttpIngress, HttpRoute, IngressListen};
use paddock_host::{AppDeployment, Runtime, RuntimeConfig};
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
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/paddock_fixture_sdk.wasm"
    );
    let component = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let server = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap();
    let (address, _rt, _ingress) = server.block_on(serve(component, pool));
    let client = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    println!("ingress_throughput: {workers} server workers, a pool of {pool}, 4 client workers");
    client.block_on(async move {
        let mut us = Vec::new();
        let mut conn = Conn::open(address).await;
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
                    let mut conn = Conn::open(address).await;
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

async fn serve(component: Vec<u8>, pool: usize) -> (SocketAddr, Runtime, HttpIngress) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 64,
        ..RuntimeConfig::default()
    })
    .unwrap();
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

/// One keep-alive connection sending `GET /plain`.
struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    async fn open(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).await.unwrap();
        stream.set_nodelay(true).unwrap();
        Self {
            stream,
            buf: Vec::with_capacity(4096),
        }
    }

    /// One request; reads the headers, then `content-length` bytes.
    async fn get(&mut self) {
        self.stream
            .write_all(b"GET /plain HTTP/1.1\r\nhost: localhost\r\n\r\n")
            .await
            .unwrap();
        loop {
            if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buf[..end]).to_ascii_lowercase();
                assert!(head.starts_with("http/1.1 200"), "{head}");
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
