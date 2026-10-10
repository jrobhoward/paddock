//! Modbus point throughput end to end, for `docs/BASELINE.md`: in-process
//! fake devices, polled by the extension over TCP on loopback, with every
//! register changing on every read (the worst case for report by
//! exception), delivered to `examples/scada/points` as a hash-routed pool
//! that writes each update into a shared `memory` bucket in one batch.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p example-scada-points
//! cargo run --release -p paddock-modbus --example modbus_load -- 16 100 10 4 4
//! ```
//!
//! Arguments: devices, registers per device (at most 125), poll interval
//! in ms, pool instances, tokio worker threads.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeMap;
use std::future::{Ready, ready};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use paddock_bundle::manifest::{
    AppInfo, AppManifest, AppSpec, ComponentRef, Durability, Provenance,
};
use paddock_host::services::Extension;
use paddock_host::{AppDeployment, Envelope, Runtime, RuntimeConfig, SharedBucket};
use paddock_modbus::{Modbus, PACKAGE};
use serde_json::json;
use tokio_modbus::server::tcp::{Server, accept_tcp_connection};
use tokio_modbus::{ExceptionCode, Request, Response};

const LONG: Duration = Duration::from_secs(30);

/// A device whose registers all change on every read.
#[derive(Clone)]
struct Churning {
    tick: Arc<AtomicU16>,
}

impl tokio_modbus::server::Service for Churning {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = Ready<Result<Response, ExceptionCode>>;

    fn call(&self, request: Self::Request) -> Self::Future {
        ready(match request {
            Request::ReadHoldingRegisters(a, n) => {
                let t = self.tick.fetch_add(1, Ordering::Relaxed);
                Ok(Response::ReadHoldingRegisters(
                    (a..a + n).map(|i| i.wrapping_add(t)).collect(),
                ))
            }
            _ => Err(ExceptionCode::IllegalFunction),
        })
    }
}

async fn device() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let service = Churning {
            tick: Arc::new(AtomicU16::new(0)),
        };
        let on_connected = |stream, addr| {
            let service = service.clone();
            async move { accept_tcp_connection(stream, addr, |_| Ok(Some(service.clone()))) }
        };
        let _ = Server::new(listener).serve(&on_connected, |_| {}).await;
    });
    address
}

fn arg(n: usize, default: u64) -> u64 {
    std::env::args()
        .nth(n)
        .map_or(default, |a| a.parse().expect("a number"))
}

fn main() {
    let workers = usize::try_from(arg(5, 4)).unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { tokio::spawn(run(workers)).await })
        .unwrap();
}

async fn run(workers: usize) {
    let devices = arg(1, 16);
    let registers = arg(2, 100);
    let poll_ms = arg(3, 10);
    let pool = arg(4, 4);
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/example_scada_points.wasm"
    );
    let component = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));

    let modbus = Modbus::default();
    let ext: Arc<dyn Extension> = Arc::new(modbus.clone());
    let rt = Runtime::with_extensions(
        RuntimeConfig {
            max_instances: 64,
            buckets: BTreeMap::from([(
                "points".to_string(),
                SharedBucket {
                    durability: Durability::Memory,
                    quota: None,
                },
            )]),
            ..RuntimeConfig::default()
        },
        vec![ext],
    )
    .unwrap();
    let app = rt.load(&component).unwrap();
    let names: Vec<String> = (0..devices).map(|i| format!("dev-{i}")).collect();
    let manifest = AppManifest {
        app: AppInfo {
            name: "points".into(),
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
            "registered_names": ["points-api"],
            "instances": { "count": pool, "routing": "hash:device" },
            "limits": { "mailbox_max": 10000 },
            "capabilities": {
                "wasi:logging": {},
                "wasi:keyvalue": { "shared": { "points": "rw" } },
                "extensions": { PACKAGE: { "write": names } }
            }
        }))
        .unwrap(),
    };
    let apps = rt
        .start_apps(vec![AppDeployment::new(manifest, app)])
        .await
        .unwrap();
    let mut configured = serde_json::Map::new();
    for name in &names {
        configured.insert(
            name.clone(),
            json!({
                "address": device().await.to_string(),
                "poll_ms": poll_ms,
                "deliver_to": "points",
                "holding": [{ "start": 0, "count": registers }]
            }),
        );
    }
    let started = rt
        .start_extensions(
            &apps,
            &BTreeMap::from([(PACKAGE.to_string(), json!({ "devices": configured }))]),
        )
        .unwrap();
    let points = apps.app("points").unwrap();
    let updates = || async {
        let mut total = 0u64;
        for instance in points.instances() {
            // A saturated pool's mailbox can be full; ask again.
            let reply = loop {
                match instance
                    .call(Envelope::new("text/plain", "updates"), LONG)
                    .await
                {
                    Ok(reply) => break reply,
                    Err(_) => tokio::time::sleep(Duration::from_millis(1)).await,
                }
            };
            total += String::from_utf8(reply.body)
                .unwrap()
                .parse::<u64>()
                .unwrap();
        }
        total
    };

    tokio::time::sleep(Duration::from_secs(2)).await;
    let (u0, t0) = (updates().await, Instant::now());
    tokio::time::sleep(Duration::from_secs(10)).await;
    let (u1, t1) = (updates().await, Instant::now());
    let secs = (t1 - t0).as_secs_f64();
    let scans = f64::from(u32::try_from(u1 - u0).unwrap()) / secs;
    let target = f64::from(u32::try_from(devices).unwrap()) * 1000.0
        / f64::from(u32::try_from(poll_ms).unwrap());
    println!(
        "{devices} devices x {registers} registers every {poll_ms} ms, pool of {pool}, {workers} workers: \
         {scans:.0} updates/s stored ({target:.0} polls/s asked), {:.0} points/s",
        scans * f64::from(u32::try_from(registers).unwrap())
    );
    started.stop();
    apps.stop().await;
}
