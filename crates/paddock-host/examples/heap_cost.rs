//! Guest heap size for `docs/BASELINE.md`: what growing an instance's heap
//! costs against native code, what a large heap holds resident, live and
//! after the instance exits, how fast its Store slot is reused, and what
//! state of that size costs a passivated instance and its wake.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock-host --example heap_cost -- [workers]
//! ```
//!
//! Per size: 16 instances each `alloc:<mib>` (allocate and touch), then
//! killed and 16 more started on the same slots; then 16 instances each
//! holding `ballast:<kib>` of the same size are passivated and woken. A
//! passivated instance keeps its snapshot in host memory.
//! Linux only for the memory figures (`/proc/self/status`).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::{Duration, Instant};

use paddock_host::{
    Envelope, InstanceHandle, InstanceSpec, LifecycleState, LoadedApp, Runtime, RuntimeConfig,
};

const INSTANCES: usize = 16;
const HEAP_MIB: [usize; 4] = [0, 1, 16, 64];
const IDLE: Duration = Duration::from_millis(20);
const LONG: Duration = Duration::from_secs(30);

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(1, |a| a.parse().expect("workers"));
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
        .block_on(async move { tokio::spawn(run(workers, component)).await })
        .unwrap();
}

fn text(body: &str) -> Envelope {
    Envelope::new("text/plain", body)
}

async fn ask(h: &InstanceHandle, body: &str) -> String {
    String::from_utf8(h.call(text(body), LONG).await.unwrap().body).unwrap()
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        memory_max: 256 << 20,
        ..InstanceSpec::new("heap", "0.1.0")
    }
}

/// `VmRSS` in MiB, if `/proc/self/status` has it.
fn rss() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
        .map_or(f64::NAN, |kib| kib / 1024.0)
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

fn median(mut samples: Vec<Duration>) -> f64 {
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1e6
}

#[allow(clippy::cast_precision_loss)]
const N: f64 = INSTANCES as f64;

async fn run(workers: usize, component: Vec<u8>) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: u32::try_from(INSTANCES + 4).unwrap(),
        max_processes: Some(u32::try_from(INSTANCES * 2).unwrap()),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    println!("tokio workers: {workers}; {INSTANCES} instances per size; medians");
    for mib in HEAP_MIB {
        heap(&rt, &app, mib).await;
    }
    for mib in HEAP_MIB {
        passivated(&rt, &app, mib).await;
    }
}

async fn heap(rt: &Runtime, app: &LoadedApp, mib: usize) {
    // Kept until all are made, so each takes fresh pages, as a guest's does.
    let native = {
        let mut samples = Vec::with_capacity(INSTANCES);
        let mut kept = Vec::with_capacity(INSTANCES);
        for _ in 0..INSTANCES {
            let t = Instant::now();
            kept.push(std::hint::black_box(vec![1u8; mib << 20]));
            samples.push(t.elapsed());
        }
        median(samples)
    };
    let before = rss();
    let mut all = Vec::with_capacity(INSTANCES);
    let mut grow = Vec::with_capacity(INSTANCES);
    for _ in 0..INSTANCES {
        let h = rt.spawn(app, spec()).unwrap();
        ask(&h, "echo:up").await;
        let t = Instant::now();
        ask(&h, &format!("alloc:{mib}")).await;
        grow.push(t.elapsed());
        all.push(h);
    }
    settle().await;
    let live = rss();
    for h in &all {
        h.kill();
        h.wait().await;
    }
    settle().await;
    let exited = rss();
    let mut respawn = Vec::with_capacity(INSTANCES);
    for _ in 0..INSTANCES {
        let t = Instant::now();
        let h = rt.spawn(app, spec()).unwrap();
        ask(&h, "echo:up").await;
        respawn.push(t.elapsed());
        h.kill();
        h.wait().await;
    }
    println!(
        "{mib:>3} MiB heap: alloc call {:.0} µs (native {native:.0} µs); {:.2} MiB resident each live, {:.2} MiB after exit; instantiate on a reused slot {:.1} µs",
        median(grow),
        (live - before) / N,
        (exited - before) / N,
        median(respawn),
    );
}

async fn passivated(rt: &Runtime, app: &LoadedApp, mib: usize) {
    let mut spec = spec();
    spec.passivate_after = Some(IDLE);
    let all: Vec<InstanceHandle> = (0..INSTANCES)
        .map(|_| rt.spawn(app, spec.clone()).unwrap())
        .collect();
    let mut snapshot = 0;
    for h in &all {
        ask(h, &format!("ballast:{}", mib << 10)).await;
        snapshot = h.snapshot().await.unwrap().len();
    }
    while all.iter().any(|h| h.state() != LifecycleState::Passive) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut wake = Vec::with_capacity(INSTANCES);
    for h in &all {
        let t = Instant::now();
        ask(h, "echo:up").await;
        wake.push(t.elapsed());
    }
    println!(
        "{mib:>3} MiB state, passivated: snapshot {} KiB held by the host; call that wakes it {:.0} µs",
        snapshot >> 10,
        median(wake),
    );
    for h in &all {
        h.kill();
        h.wait().await;
    }
    settle().await;
}
