//! What a guest log record costs, for `docs/BASELINE.md`: one below the
//! app's level (filtered) and one at or above it (emitted, with no tracing
//! subscriber installed, so the host's own formatting is not counted).
//!
//! The guest is `examples/chaos`, built with `paddock-sdk`; its `log:<n>`
//! command writes `n` debug records with `paddock_sdk::debug!`.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p example-chaos
//! cargo run --release -p paddock-host --example log_cost
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::{Duration, Instant};

use paddock_bundle::manifest::{Capability, LogLevel};
use paddock_host::{Envelope, InstanceHandle, InstanceSpec, Runtime, RuntimeConfig};

const RECORDS: u32 = 100_000;
const LONG: Duration = Duration::from_secs(30);

fn main() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/example_chaos.wasm"
    );
    let component = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(run(component)).await })
        .unwrap();
}

async fn per_record(h: &InstanceHandle) -> Duration {
    let call = |body: String| h.call(Envelope::new("text/plain", body), LONG);
    // Warm up, and let any host event after `init` arrive.
    call("log:1000".into()).await.unwrap();
    let mut samples = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        call("log:0".into()).await.unwrap();
        let empty = started.elapsed();
        let started = Instant::now();
        call(format!("log:{RECORDS}")).await.unwrap();
        samples.push(started.elapsed().saturating_sub(empty) / RECORDS);
    }
    samples.sort();
    samples[samples.len() / 2]
}

async fn run(component: Vec<u8>) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 8,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    for (label, level) in [
        ("filtered (debug record, level info)", LogLevel::Info),
        ("emitted (debug record, level debug)", LogLevel::Debug),
    ] {
        let mut spec = InstanceSpec::new("chaos", "1.0.0");
        spec.callback_deadline = LONG;
        spec.granted.insert(Capability::Logging);
        spec.services.logging.level = level;
        let h = rt.spawn(&app, spec).unwrap();
        let cost = per_record(&h).await;
        println!(
            "{label}: {:.0} ns per record (median of 5 runs of {RECORDS})",
            cost.as_secs_f64() * 1e9
        );
        h.kill();
    }
}
