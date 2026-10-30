//! How long a message waits when its sender keeps running: instance A casts
//! to instance B and then runs guest code in the same poll of its task. A
//! task woken by a running task goes into that worker's LIFO slot, which
//! other workers do not steal from, so B may wait for A to yield (at the
//! next epoch tick) even with idle workers.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock-host --example wake_cost -- [workers]
//! ```
//!
//! Each round starts a fresh B and casts A two commands: `cast:<B>:trap`,
//! then (except in the control) `busy:<n>`. A drains both in one poll of
//! its task, since a receive from a non-empty mailbox does not suspend. The
//! time is from the host's first cast to B's exit, so it includes A's wake,
//! B's trap and its exit (the control shows that floor). The reply cases
//! time a call from the host to A with `busy:<n>` queued behind it.
//!
//! Built with `RUSTFLAGS="--cfg tokio_unstable"` (in its own target
//! directory), `WAKE_NO_LIFO=1` turns tokio's LIFO slot off for comparison.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::{Duration, Instant};

use paddock_host::{Envelope, InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

const ROUNDS: usize = 200;
const LONG: Duration = Duration::from_secs(10);

// `tokio_unstable` is set only for the comparison build.
#[allow(unexpected_cfgs)]
fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(16, |a| a.parse().expect("workers"));
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/fixtures/wasm32-wasip2/release/paddock_fixture_chaos.wasm"
    );
    let component = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(workers).enable_all();
    // Built with `--cfg tokio_unstable`, `WAKE_NO_LIFO=1` turns tokio's
    // LIFO slot off, to check that it is what B waits behind.
    #[cfg(tokio_unstable)]
    if std::env::var_os("WAKE_NO_LIFO").is_some() {
        builder.disable_lifo_slot();
        println!("LIFO slot disabled");
    }
    builder
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
        callback_deadline: Duration::from_secs(5),
        ..InstanceSpec::new("wake", "0.1.0")
    }
}

/// `busy` rounds that take about `ms` on this machine, from a timed call.
async fn rounds_for(a: &InstanceHandle, ms: f64) -> u64 {
    let probe = 2_000_000u64;
    ask(a, &format!("busy:{probe}")).await;
    let started = Instant::now();
    ask(a, &format!("busy:{probe}")).await;
    let per_round = started.elapsed().as_secs_f64() / probe as f64;
    (ms / 1000.0 / per_round) as u64
}

fn percentile(sorted: &[Duration], p: f64) -> f64 {
    let i = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[i].as_secs_f64() * 1e6
}

async fn case(rt: &Runtime, app: &LoadedApp, a: &InstanceHandle, label: &str, busy: Option<u64>) {
    let mut waits = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let b = rt.spawn(app, spec()).unwrap();
        ask(&b, "echo:up").await;
        let wake = format!("cast:{}:trap", b.pid().text());

        let started = Instant::now();
        a.cast(text(&wake)).unwrap();
        if let Some(n) = busy {
            a.cast(text(&format!("busy:{n}"))).unwrap();
        }
        b.wait().await;
        waits.push(started.elapsed());
        // Let A finish before the next round.
        ask(a, "echo:idle").await;
    }
    waits.sort();
    println!(
        "{label:<40} {:>8.0} {:>8.0} {:>8.0} {:>8.0} {:>8.0}",
        percentile(&waits, 0.10),
        percentile(&waits, 0.50),
        percentile(&waits, 0.90),
        percentile(&waits, 0.99),
        percentile(&waits, 1.0),
    );
}

/// A reply wakes its caller the same way: the host calls A, and A has
/// `busy:<n>` queued behind the call. The time is the call's round trip.
async fn reply_case(a: &InstanceHandle, label: &str, busy: Option<u64>) {
    let mut waits = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let call = a.call(text("echo:x"), LONG);
        let started = Instant::now();
        let reply = async {
            let r = call.await;
            (r, started.elapsed())
        };
        let (result, took) = if let Some(n) = busy {
            let (r, ()) = tokio::join!(reply, async {
                a.cast(text(&format!("busy:{n}"))).unwrap();
            });
            r
        } else {
            reply.await
        };
        result.unwrap();
        waits.push(took);
        ask(a, "echo:idle").await;
    }
    waits.sort();
    println!(
        "{label:<40} {:>8.0} {:>8.0} {:>8.0} {:>8.0} {:>8.0}",
        percentile(&waits, 0.10),
        percentile(&waits, 0.50),
        percentile(&waits, 0.90),
        percentile(&waits, 0.99),
        percentile(&waits, 1.0),
    );
}

async fn run(workers: usize, component: Vec<u8>) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    let a = rt.spawn(&app, spec()).unwrap();
    ask(&a, "echo:up").await;
    let ms1 = rounds_for(&a, 1.0).await;
    println!(
        "tokio workers: {workers}; epoch tick {:?}; {ROUNDS} rounds; cast to B's exit in µs",
        RuntimeConfig::default().epoch_tick
    );
    println!(
        "{:<40} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "case", "p10", "p50", "p90", "p99", "max"
    );
    case(&rt, &app, &a, "control: A idle after the cast", None).await;
    for ms in [0.5, 1.0, 5.0, 20.0] {
        let label = format!("A runs {ms} ms after the cast");
        case(&rt, &app, &a, &label, Some((ms * ms1 as f64) as u64)).await;
    }
    reply_case(&a, "reply: A idle after the call", None).await;
    reply_case(&a, "reply: A runs 5 ms after the call", Some(5 * ms1)).await;
}
