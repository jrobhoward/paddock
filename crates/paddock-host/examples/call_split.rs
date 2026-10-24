//! Where an empty call's time goes, for `docs/BASELINE.md`: sequential calls
//! to one chaos instance against a bare tokio request and reply of the same
//! shape (a task with a bounded mailbox, a oneshot reply). For each: wall
//! time, on-CPU time summed over the process's threads, and context
//! switches, per call (Linux, `/proc/self/task`). For the Paddock call, the
//! share inside the callback (guest code, wasmtime's entry and exit, the
//! canonical ABI) from the callback clock, through the
//! `paddock_callback_guest_seconds` metric.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock-host --example call_split -- [workers] [paddock]
//! ```
//!
//! `paddock` skips the bare tokio run, for a profile of Paddock's calls
//! alone (`perf record`).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::{Duration, Instant};

use paddock_host::{Envelope, InstanceSpec, Runtime, RuntimeConfig};
use tokio::sync::{mpsc, oneshot};

const CALLS: usize = 200_000;
const LONG: Duration = Duration::from_secs(10);

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(1, |a| a.parse().expect("workers"));
    let bare = std::env::args().nth(2).as_deref() != Some("paddock");
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/fixtures/wasm32-wasip2/release/paddock_fixture_chaos.wasm"
    );
    let component = std::fs::read(path).unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(run(workers, bare, component)).await })
        .unwrap();
}

/// (on-CPU ns summed over threads, voluntary switches, involuntary switches)
fn usage() -> (u64, u64, u64) {
    let (mut cpu, mut vol, mut invol) = (0, 0, 0);
    for t in std::fs::read_dir("/proc/self/task").unwrap() {
        let dir = t.unwrap().path();
        let sched = std::fs::read_to_string(dir.join("schedstat")).unwrap_or_default();
        cpu += sched
            .split_whitespace()
            .next()
            .map_or(0, |v| v.parse().unwrap());
        for line in std::fs::read_to_string(dir.join("status"))
            .unwrap_or_default()
            .lines()
        {
            let n = |l: &str| l.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap();
            if line.starts_with("voluntary_ctxt_switches") {
                vol += n(line);
            } else if line.starts_with("nonvoluntary_ctxt_switches") {
                invol += n(line);
            }
        }
    }
    (cpu, vol, invol)
}

fn guest_sum(rt: &Runtime) -> (f64, f64) {
    let text = rt.render_metrics();
    let get = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.rsplit(' ').next())
            .map_or(0.0, |v| v.parse().unwrap())
    };
    (
        get("paddock_callback_guest_seconds_sum"),
        get("paddock_callback_guest_seconds_count"),
    )
}

#[allow(clippy::cast_precision_loss)]
fn report(label: &str, wall: Duration, before: (u64, u64, u64), after: (u64, u64, u64)) {
    let per = |x: f64| x / CALLS as f64;
    println!(
        "{label}: wall {:.2} µs/call, on-CPU {:.2} µs/call (all threads), switches/call {:.2} voluntary, {:.3} involuntary",
        per(wall.as_secs_f64() * 1e6),
        per((after.0 - before.0) as f64 / 1e3),
        per((after.1 - before.1) as f64),
        per((after.2 - before.2) as f64),
    );
}

async fn run(workers: usize, bare: bool, component: Vec<u8>) {
    println!("tokio workers: {workers}, {CALLS} sequential calls");
    // Bare tokio: a server task with a mailbox, a oneshot reply.
    let (tx, mut rx) = mpsc::channel::<(Vec<u8>, oneshot::Sender<Vec<u8>>)>(1024);
    tokio::spawn(async move {
        while let Some((body, reply)) = rx.recv().await {
            let _ = reply.send(body);
        }
    });
    if bare {
        for _ in 0..10_000 {
            let (r, w) = oneshot::channel();
            tx.send((b"x".to_vec(), r)).await.unwrap();
            w.await.unwrap();
        }
        let b = usage();
        let t = Instant::now();
        for _ in 0..CALLS {
            let (r, w) = oneshot::channel();
            tx.send((b"x".to_vec(), r)).await.unwrap();
            w.await.unwrap();
        }
        report("bare tokio mpsc + oneshot", t.elapsed(), b, usage());
    }

    let rt = Runtime::new(RuntimeConfig {
        max_instances: 8,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    let h = rt.spawn(&app, InstanceSpec::new("split", "0.1.0")).unwrap();
    let msg = || Envelope::new("text/plain", "echo:x");
    for _ in 0..10_000 {
        h.call(msg(), LONG).await.unwrap();
    }
    let g0 = guest_sum(&rt);
    let b = usage();
    let t = Instant::now();
    for _ in 0..CALLS {
        h.call(msg(), LONG).await.unwrap();
    }
    let wall = t.elapsed();
    let a = usage();
    let g1 = guest_sum(&rt);
    report("paddock empty call", wall, b, a);
    #[allow(clippy::cast_precision_loss)]
    let wall_us = wall.as_secs_f64() * 1e6 / CALLS as f64;
    let guest_us = (g1.0 - g0.0) * 1e6 / (g1.1 - g0.1);
    println!(
        "  of which in the callback (guest code, wasmtime entry/exit, canonical ABI): {guest_us:.2} µs; outside it: {:.2} µs",
        wall_us - guest_us
    );
}
