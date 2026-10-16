//! Points per second from one app to another, one cast per point against
//! batches through `paddock_sdk::batch` (ROADMAP F4), for
//! `docs/BASELINE.md`.
//!
//! Two apps of `paddock-fixture-sdk`: `source` instances each send N sample
//! points (device, address, value, time; about 25 bytes in CBOR) to a
//! `sink` instance (`produce:sink:<n>:<batch>`, batch 0 for one cast per
//! point), and `sink` instances count them. The clock runs from the first
//! `produce` call until every point has been counted, so it covers
//! encoding, delivery and decoding end to end. The sink's mailbox holds
//! every message, so nothing is dropped.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-sdk
//! cargo run --release -p paddock-host --example batch_cost -- 4
//! ```
//!
//! Argument: tokio worker threads (default 4).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::{Duration, Instant};

use paddock_bundle::manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};
use paddock_host::{AppDeployment, Envelope, InstanceHandle, LoadedApp, Runtime, RuntimeConfig};
use serde_json::json;

const LONG: Duration = Duration::from_secs(120);
const POINTS: u32 = 200_000;
const BATCHES: [usize; 5] = [0, 1, 10, 100, 1_000];

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |w| w.parse().expect("worker threads"));
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/paddock_fixture_sdk.wasm"
    );
    let component = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(run(component, workers)).await })
        .unwrap();
}

fn deployment(name: &str, app: &LoadedApp, pool: u32) -> AppDeployment {
    let spec = json!({
        "instances": { "count": pool },
        "limits": {
            "memory_max": "256MiB",
            "callback_deadline_ms": 120000,
            "mailbox_max": 1000000
        }
    });
    AppDeployment::new(
        AppManifest {
            app: AppInfo {
                name: name.into(),
                version: "0.1.0".parse().unwrap(),
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
            spec: AppSpec::from_json_value(&spec).unwrap(),
        },
        app.clone(),
    )
}

async fn ask(h: &InstanceHandle, command: &str) -> String {
    let reply = h
        .call(Envelope::new("text/plain", command), LONG)
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

async fn counted(sinks: &[InstanceHandle]) -> u64 {
    let mut total = 0;
    for s in sinks {
        total += ask(s, "points").await.parse::<u64>().unwrap();
    }
    total
}

async fn run(component: Vec<u8>, workers: usize) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 64,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    println!("tokio workers: {workers}; {POINTS} points per source");
    println!("sources  sinks  batch  points/s     messages/s  time");
    for pool in [1u32, 4] {
        let apps = rt
            .start_apps(vec![
                deployment("sink", &app, pool),
                deployment("source", &app, pool),
            ])
            .await
            .unwrap();
        let sinks = apps.app("sink").unwrap().instances();
        let sources = apps.app("source").unwrap().instances();
        for batch in BATCHES {
            let before = counted(&sinks).await;
            let expected = before + u64::from(POINTS) * u64::from(pool);
            let started = Instant::now();
            let tasks: Vec<_> = sources
                .iter()
                .map(|s| {
                    let (s, command) = (s.clone(), format!("produce:sink:{POINTS}:{batch}"));
                    tokio::spawn(async move { ask(&s, &command).await })
                })
                .collect();
            for t in tasks {
                assert_eq!(t.await.unwrap(), "ok");
            }
            let mut now = counted(&sinks).await;
            while now < expected {
                tokio::time::sleep(Duration::from_millis(1)).await;
                now = counted(&sinks).await;
            }
            let took = started.elapsed();
            let points = f64::from(POINTS) * f64::from(pool);
            let messages = if batch == 0 {
                points
            } else {
                (points / batch as f64).ceil()
            };
            println!(
                "{pool:<8} {pool:<6} {:<6} {:<12.0} {:<11.0} {:.1} ms",
                if batch == 0 {
                    "none".to_string()
                } else {
                    batch.to_string()
                },
                points / took.as_secs_f64(),
                messages / took.as_secs_f64(),
                took.as_secs_f64() * 1e3
            );
        }
        apps.stop().await;
    }
}
