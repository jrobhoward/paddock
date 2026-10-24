//! Timer and clock accuracy for `docs/BASELINE.md`: how late a
//! `process.send-after` timer and a guest's `wasi:clocks` sleep are handled,
//! against a native tokio sleep on the same runtime, on an idle runtime and
//! under load; and what a monotonic clock read costs in a guest.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock-host --example timer_cost -- [workers]
//! ```
//!
//! Lateness is the time from when a timer was due to when its code ran: for
//! `send-after`, the guest's own clock in `handle-info`; for a guest sleep,
//! the call's time less the sleep, so it includes one call (about 3 µs).
//! The load is 64 clients calling 16 instances with `busy:20000` (about
//! 31 µs of guest work) back to back.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use paddock_host::{Envelope, InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

const TIMERS: usize = 100;
const ROUNDS: usize = 10;
const DELAYS_MS: [u64; 3] = [1, 10, 50];
const CLOCK_READS: u64 = 1_000_000;
const LOAD_INSTANCES: usize = 16;
const LOAD_CLIENTS: usize = 64;
const LONG: Duration = Duration::from_secs(10);

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |a| a.parse().expect("workers"));
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

async fn run(workers: usize, component: Vec<u8>) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: u32::try_from(TIMERS + LOAD_INSTANCES + 8).unwrap(),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    let spec = InstanceSpec::new("timer", "0.1.0");
    let timers: Vec<InstanceHandle> = (0..TIMERS)
        .map(|_| rt.spawn(&app, spec.clone()).unwrap())
        .collect();
    for h in &timers {
        ask(h, "echo:up").await;
    }
    println!(
        "tokio workers: {workers}; {TIMERS} timers at once, {ROUNDS} rounds per delay; lateness in µs"
    );

    clock_reads(&timers[0]).await;
    println!(
        "{:<34} {:>6} {:>8} {:>8} {:>8}",
        "", "delay", "p50", "p99", "max"
    );
    measure(&timers, "idle").await;
    let stop = load(&rt, &app);
    // Let the clients fill the instances' mailboxes.
    tokio::time::sleep(Duration::from_millis(200)).await;
    measure(&timers, "loaded").await;
    stop.store(true, Ordering::Relaxed);
    for h in &timers {
        h.kill();
    }
}

async fn clock_reads(h: &InstanceHandle) {
    let native = {
        let started = Instant::now();
        for _ in 0..CLOCK_READS {
            std::hint::black_box(Instant::now());
        }
        started.elapsed()
    };
    let guest_ns: f64 = ask(h, &format!("clock:{CLOCK_READS}"))
        .await
        .parse()
        .unwrap();
    #[allow(clippy::cast_precision_loss)]
    let reads = CLOCK_READS as f64;
    println!(
        "monotonic clock read: native {:.1} ns, guest {:.1} ns (one run of {CLOCK_READS} reads)",
        native.as_secs_f64() * 1e9 / reads,
        guest_ns / reads,
    );
}

/// Start the load; it runs until the returned flag is set.
fn load(rt: &Runtime, app: &LoadedApp) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let busy: Vec<InstanceHandle> = (0..LOAD_INSTANCES)
        .map(|_| rt.spawn(app, InstanceSpec::new("load", "0.1.0")).unwrap())
        .collect();
    let busy = Arc::new(busy);
    for client in 0..LOAD_CLIENTS {
        let (busy, stop) = (busy.clone(), stop.clone());
        tokio::spawn(async move {
            let h = &busy[client % busy.len()];
            while !stop.load(Ordering::Relaxed) {
                let _ = h.call(text("busy:20000"), LONG).await;
            }
            if client < busy.len() {
                busy[client].kill();
            }
        });
    }
    stop
}

async fn measure(timers: &[InstanceHandle], condition: &str) {
    for ms in DELAYS_MS {
        let delay = Duration::from_millis(ms);
        let mut native = Vec::new();
        let mut send_after = Vec::new();
        let mut sleep = Vec::new();
        for _ in 0..ROUNDS {
            native.extend(native_round(delay).await);
            send_after.extend(send_after_round(timers, delay).await);
            sleep.extend(sleep_round(timers, delay).await);
        }
        show(&format!("{condition}, native tokio sleep"), ms, native);
        show(&format!("{condition}, guest send-after"), ms, send_after);
        show(&format!("{condition}, guest wasi:clocks sleep"), ms, sleep);
    }
}

async fn native_round(delay: Duration) -> Vec<u64> {
    let tasks: Vec<_> = (0..TIMERS)
        .map(|_| {
            tokio::spawn(async move {
                let due = Instant::now() + delay;
                tokio::time::sleep(delay).await;
                micros(Instant::now().saturating_duration_since(due))
            })
        })
        .collect();
    let mut late = Vec::with_capacity(TIMERS);
    for t in tasks {
        late.push(t.await.unwrap());
    }
    late
}

async fn send_after_round(timers: &[InstanceHandle], delay: Duration) -> Vec<u64> {
    let ms = delay.as_millis();
    for h in timers {
        ask(h, &format!("timed:{ms}")).await;
    }
    tokio::time::sleep(delay + Duration::from_millis(50)).await;
    let mut late = Vec::with_capacity(timers.len());
    for h in timers {
        let reply = ask(h, "lateness").await;
        late.extend(
            reply
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<u64>().unwrap()),
        );
    }
    assert_eq!(late.len(), timers.len(), "every timer fired once");
    late
}

async fn sleep_round(timers: &[InstanceHandle], delay: Duration) -> Vec<u64> {
    let ms = delay.as_millis();
    let tasks: Vec<_> = timers
        .iter()
        .map(|h| {
            let h = h.clone();
            tokio::spawn(async move {
                let started = Instant::now();
                ask(&h, &format!("sleep:{ms}")).await;
                micros(started.elapsed().saturating_sub(delay))
            })
        })
        .collect();
    let mut late = Vec::with_capacity(timers.len());
    for t in tasks {
        late.push(t.await.unwrap());
    }
    late
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn show(label: &str, ms: u64, mut late: Vec<u64>) {
    late.sort_unstable();
    let at = |q: f64| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let i = ((late.len() as f64 - 1.0) * q).round() as usize;
        late[i]
    };
    println!(
        "{label:<34} {:>4} ms {:>8} {:>8} {:>8}",
        ms,
        at(0.5),
        at(0.99),
        late.last().unwrap()
    );
}
