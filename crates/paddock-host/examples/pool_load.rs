//! Many concurrent clients against a pool of instances, for
//! `docs/BASELINE.md`.
//!
//! Starts the chaos fixture as an app with a pool of P instances
//! (round-robin routing), runs C client tasks that each call the app back to
//! back for a fixed time, and reports aggregate calls per second and latency.
//! Each call is either empty (`echo:x`) or does guest work (`busy:20000`,
//! about 31 µs on the machine in BASELINE.md).
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock-host --example pool_load -- [workers] [epoch_tick_us]
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::{Duration, Instant};

use paddock_bundle::manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};
use paddock_host::{AppDeployment, Envelope, LoadedApp, Runtime, RuntimeConfig};
use serde_json::json;

const RUN: Duration = Duration::from_secs(1);
const LONG: Duration = Duration::from_secs(10);
const WORK: [(&str, &str); 2] = [("empty", "echo:x"), ("busy:20000", "busy:20000")];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let workers: usize = args.first().map_or(1, |a| a.parse().expect("workers"));
    let tick = args
        .get(1)
        .map_or(RuntimeConfig::default().epoch_tick, |a| {
            Duration::from_micros(a.parse().expect("epoch_tick_us"))
        });
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/fixtures/wasm32-wasip2/release/paddock_fixture_chaos.wasm"
    );
    let component = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(run(workers, tick, component)).await })
        .unwrap();
}

fn deployment(name: &str, app: &LoadedApp, pool: u32) -> AppDeployment {
    let spec = json!({
        "instances": { "count": pool },
        "limits": { "memory_max": "32MiB", "callback_deadline_ms": 5000 }
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

fn quantile(sorted: &[Duration], q: f64) -> Duration {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let i = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[i]
}

fn us(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1e6)
}

async fn run(workers: usize, tick: Duration, component: Vec<u8>) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 64,
        epoch_tick: tick,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    println!(
        "tokio workers: {workers}; epoch tick {} µs; {} s per row",
        tick.as_micros(),
        RUN.as_secs()
    );
    println!("work        pool  clients  calls/s   mean µs  p50 µs  p99 µs  p99.9 µs");
    let mut configs = Vec::new();
    for pool in [1, 4, 16] {
        configs.push((pool, 64));
    }
    for clients in [1, 16, 256] {
        configs.push((16, clients));
    }
    for (label, body) in WORK {
        for (i, &(pool, clients)) in configs.iter().enumerate() {
            let name = format!("load-{i}-{}", label.len());
            let apps = rt
                .start_apps(vec![deployment(&name, &app, pool)])
                .await
                .unwrap();
            let handle = apps.app(&name).unwrap();
            // Warm every instance.
            for _ in 0..pool * 2 {
                handle
                    .call(Envelope::new("text/plain", body), LONG)
                    .await
                    .unwrap();
            }
            let deadline = Instant::now() + RUN;
            let tasks: Vec<_> = (0..clients)
                .map(|_| {
                    let handle = handle.clone();
                    tokio::spawn(async move {
                        let mut samples = Vec::new();
                        while Instant::now() < deadline {
                            let t = Instant::now();
                            handle
                                .call(Envelope::new("text/plain", body), LONG)
                                .await
                                .unwrap();
                            samples.push(t.elapsed());
                        }
                        samples
                    })
                })
                .collect();
            let started = Instant::now();
            let mut samples = Vec::new();
            for t in tasks {
                samples.extend(t.await.unwrap());
            }
            let elapsed = started.elapsed();
            samples.sort();
            #[allow(clippy::cast_precision_loss)]
            let rate = samples.len() as f64 / elapsed.as_secs_f64();
            let mean = samples.iter().sum::<Duration>() / u32::try_from(samples.len()).unwrap();
            println!(
                "{label:<11} {pool:>4}  {clients:>7}  {rate:>8.0}  {:>7}  {:>6}  {:>6}  {:>8}",
                us(mean),
                us(quantile(&samples, 0.5)),
                us(quantile(&samples, 0.99)),
                us(quantile(&samples, 0.999)),
            );
            apps.stop().await;
        }
    }
}
