//! What passivation costs and buys (spike S11): many more instances than
//! Store slots, the memory each passivated instance holds, the time a call
//! takes when it has to wake its instance, and calls spread at random over
//! every instance.
//!
//! The guest is `paddock-fixture-counter` (v1): `inc` and `get`, its
//! snapshot the count in decimal.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-counter
//! cargo run --release -p paddock-host --example passivation_cost -- 4 1000 20000
//! ```
//!
//! Arguments: tokio worker threads (default 4), Store slots
//! (`max_instances`, default 1,000) and instances (default 20,000); a
//! fourth, `off`, runs the same without passivation, for comparison (no
//! more instances than slots).
//!
//! ```sh
//! cargo run --release -p paddock-host --example passivation_cost -- 4 1000 1000 off
//! ```
//! Memory figures read `/proc/self/status` (Linux only).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use paddock_host::{
    Envelope, InstanceHandle, InstanceSpec, LifecycleState, LoadedApp, Runtime, RuntimeConfig,
};

const LONG: Duration = Duration::from_secs(60);
const IDLE: Duration = Duration::from_millis(20);
const WAKES: usize = 2_000;
const CHURN: Duration = Duration::from_secs(5);
const CALLERS: usize = 64;

fn arg(n: usize, default: usize) -> usize {
    std::env::args()
        .nth(n)
        .map_or(default, |a| a.parse().expect("a number"))
}

fn main() {
    let workers = arg(1, 4);
    let slots = arg(2, 1_000);
    let processes = arg(3, 20_000);
    let passivate = std::env::args().nth(4).as_deref() != Some("off");
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/paddock_fixture_counter.wasm"
    );
    let component = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            tokio::spawn(run(component, workers, slots, processes, passivate)).await
        })
        .unwrap();
}

/// `VmRSS` and `VmSize` in MiB, if `/proc/self/status` has them.
fn memory() -> Option<(f64, f64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| -> Option<f64> {
        let line = status.lines().find(|l| l.starts_with(name))?;
        let kib: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib / 1024.0)
    };
    Some((field("VmRSS:")?, field("VmSize:")?))
}

fn show_memory(when: &str) -> Option<(f64, f64)> {
    let m = memory();
    match m {
        Some((rss, size)) => println!("{when}: RSS {rss:.0} MiB, address space {size:.0} MiB"),
        None => println!("{when}: memory not available"),
    }
    m
}

async fn ask(h: &InstanceHandle, body: &str) -> String {
    let reply = h
        .call(Envelope::new("text/plain", body.to_string()), LONG)
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

fn percentiles(label: &str, mut us: Vec<f64>) {
    us.sort_by(f64::total_cmp);
    let at = |p: f64| us[((us.len() - 1) as f64 * p) as usize];
    println!(
        "{label}: p50 {:.0} us, p90 {:.0} us, p99 {:.0} us, max {:.0} us ({} calls)",
        at(0.5),
        at(0.9),
        at(0.99),
        us[us.len() - 1],
        us.len()
    );
}

async fn until_passive(all: &[InstanceHandle]) -> Duration {
    let started = Instant::now();
    loop {
        let live = all
            .iter()
            .filter(|h| h.state() != LifecycleState::Passive)
            .count();
        if live == 0 {
            return started.elapsed();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run(component: Vec<u8>, workers: usize, slots: usize, processes: usize, passivate: bool) {
    println!(
        "passivation_cost: {workers} workers, {slots} Store slots, {processes} instances, {}",
        if passivate {
            format!("passivated after {} ms idle", IDLE.as_millis())
        } else {
            "no passivation".into()
        }
    );
    let rt = Runtime::new(RuntimeConfig {
        max_instances: u32::try_from(slots).unwrap(),
        max_processes: Some(u32::try_from(processes).unwrap()),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app: LoadedApp = rt.load(&component).unwrap();
    let before = show_memory("runtime started");

    let mut spec = InstanceSpec::new("counter", "1.0.0");
    spec.passivate_after = passivate.then_some(IDLE);
    let started = Instant::now();
    let all: Vec<InstanceHandle> = (0..processes)
        .map(|_| rt.spawn(&app, spec.clone()).unwrap())
        .collect();
    if passivate {
        until_passive(&all).await;
    } else {
        for h in &all {
            ask(h, "get").await;
        }
    }
    println!(
        "{processes} instances started{} in {:.1} s; Stores held: {}",
        if passivate { " and passivated" } else { "" },
        started.elapsed().as_secs_f64(),
        rt.store_count()
    );
    let state = if passivate { "passivated" } else { "idle live" };
    if let (Some((rss0, size0)), Some((rss1, size1))) =
        (before, show_memory(&format!("all {state}")))
    {
        println!(
            "per {state} instance: {:.1} KiB RSS, {:.1} KiB address space",
            (rss1 - rss0) * 1024.0 / processes as f64,
            (size1 - size0) * 1024.0 / processes as f64
        );
    }

    wake_latency(&all, passivate).await;
    let all = Arc::new(all);
    let random = random_calls(&all).await;
    show_memory("after random calls");

    let total: u64 = {
        let mut sum = 0;
        for h in all.iter() {
            sum += ask(h, "get")
                .await
                .rsplit('=')
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap();
        }
        sum
    };
    let expected = 2 * WAKES.min(processes) as u64 + random;
    println!("counts kept: {total} of {expected} increments");
    assert_eq!(total, expected, "an increment was lost");
    for h in all.iter() {
        h.kill();
    }
}

/// A call to a passivated instance, then a second call while it is still
/// live, on [`WAKES`] instances spread over `all`.
async fn wake_latency(all: &[InstanceHandle], passivate: bool) {
    let mut woken = Vec::with_capacity(WAKES);
    let mut live = Vec::with_capacity(WAKES);
    let step = (all.len() / WAKES).max(1);
    for h in all.iter().step_by(step).take(WAKES) {
        let t = Instant::now();
        ask(h, "inc").await;
        woken.push(t.elapsed().as_secs_f64() * 1e6);
        let t = Instant::now();
        ask(h, "inc").await;
        live.push(t.elapsed().as_secs_f64() * 1e6);
    }
    if passivate {
        percentiles("call that wakes its instance", woken);
    }
    percentiles("call to a live instance", live);
    if passivate {
        until_passive(all).await;
    }
}

/// [`CALLERS`] callers each calling `inc` on instances picked at random,
/// for [`CHURN`]; how many calls were made.
async fn random_calls(all: &Arc<Vec<InstanceHandle>>) -> u64 {
    let done = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + CHURN;
    let mut tasks = Vec::new();
    for c in 0..CALLERS {
        let (all, done) = (all.clone(), done.clone());
        tasks.push(tokio::spawn(async move {
            let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ (c as u64 + 1);
            let mut us = Vec::new();
            while Instant::now() < deadline {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let h = &all[(x % all.len() as u64) as usize];
                let t = Instant::now();
                ask(h, "inc").await;
                us.push(t.elapsed().as_secs_f64() * 1e6);
                done.fetch_add(1, Ordering::Relaxed);
            }
            us
        }));
    }
    let mut us = Vec::new();
    for t in tasks {
        us.extend(t.await.unwrap());
    }
    let done = done.load(Ordering::Relaxed);
    println!(
        "random calls, {CALLERS} callers: {:.0} calls/s",
        done as f64 / CHURN.as_secs_f64()
    );
    percentiles("random call", us);
    done
}
