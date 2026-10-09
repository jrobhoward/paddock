//! What keyvalue watches cost, for `docs/BASELINE.md`: the time of one
//! write with 0 to 1,000 instances watching the key (or watching another
//! key), and the time from starting a write until a watcher has handled its
//! notice.
//!
//! The guest is `paddock-fixture-sdk`: `set:<bucket>:<key>:<value>` writes
//! with `wasi:keyvalue`, `watch:<bucket>:<key>` watches with
//! `paddock:watch`, and `changes` lists the notices it has handled. The
//! bucket is a shared `memory` bucket, so storage is a map insert.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-sdk
//! cargo run --release -p paddock-host --example watch_cost -- 4
//! ```
//!
//! Argument: tokio worker threads (default 4).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use paddock_bundle::manifest::{AccessMode, Capability, Durability};
use paddock_host::{
    Envelope, InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig, SharedBucket,
};

const LONG: Duration = Duration::from_secs(30);
const WRITES: usize = 2_000;
const TRIALS: usize = 1_000;

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

async fn ask(h: &InstanceHandle, body: &str) -> String {
    let reply = h
        .call(Envelope::new("text/plain", body.to_string()), LONG)
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

fn spawn(rt: &Runtime, app: &LoadedApp, name: &str) -> InstanceHandle {
    let mut spec = InstanceSpec::new(name, "1.0.0");
    spec.callback_deadline = LONG;
    spec.granted.insert(Capability::KeyValue);
    spec.services.keyvalue.shared = BTreeMap::from([("board".to_string(), AccessMode::Rw)]);
    rt.spawn(app, spec).unwrap()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// Median time of one `set` call from the writer.
async fn write_cost(writer: &InstanceHandle, key: &str) -> Duration {
    let mut samples = Vec::with_capacity(WRITES);
    for i in 0..WRITES {
        let started = Instant::now();
        ask(writer, &format!("set:board:{key}:{i}")).await;
        samples.push(started.elapsed());
    }
    median(samples)
}

async fn run(component: Vec<u8>, workers: usize) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 1_100,
        buckets: BTreeMap::from([(
            "board".to_string(),
            SharedBucket {
                durability: Durability::Memory,
                quota: None,
            },
        )]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    let writer = spawn(&rt, &app, "writer");
    println!("{workers} worker threads; median of {WRITES} writes each");

    // Write cost against the number of watchers on the key.
    let mut watchers: Vec<InstanceHandle> = Vec::new();
    for target in [0usize, 1, 100, 1_000] {
        while watchers.len() < target {
            let w = spawn(&rt, &app, "watcher");
            ask(&w, "watch:board:hot").await;
            watchers.push(w);
        }
        let cost = write_cost(&writer, "hot").await;
        println!(
            "write, {target:>4} watchers on the key: {:.1} µs",
            micros(cost)
        );
    }
    let cold = write_cost(&writer, "cold").await;
    println!(
        "write, 1000 watchers on another key: {:.1} µs",
        micros(cold)
    );
    for w in watchers.drain(..) {
        w.kill();
    }

    // Write to notice: start a write, then poll the watcher until it has
    // handled the notice. Signals are taken before calls, so each poll sees
    // every notice queued before it.
    let watcher = spawn(&rt, &app, "watcher");
    ask(&watcher, "watch:board:latency").await;
    let mut plain = Vec::with_capacity(TRIALS);
    let mut latency = Vec::with_capacity(TRIALS);
    for i in 0..TRIALS {
        let started = Instant::now();
        ask(&watcher, "rev:board").await;
        ask(&watcher, "rev:board").await;
        plain.push(started.elapsed());

        let started = Instant::now();
        ask(&writer, &format!("set:board:latency:{i}")).await;
        loop {
            let changes = ask(&watcher, "changes").await;
            if changes.split(',').count() > i {
                break;
            }
        }
        latency.push(started.elapsed());
    }
    println!(
        "write to notice handled: {:.1} µs (two plain calls: {:.1} µs; median of {TRIALS})",
        micros(median(latency)),
        micros(median(plain))
    );
    watcher.kill();
    writer.kill();
}
