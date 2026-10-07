//! Spike S8: how much slower is work inside a Paddock instance than the same
//! code compiled natively, and what does a call into an instance cost?
//!
//! usage: s8-bench <s8_guest.wasm> [workers]

use std::hint::black_box;
use std::time::{Duration, Instant};

use paddock_host::{Envelope, InstanceHandle, InstanceSpec, Runtime, RuntimeConfig};

const RUNS: usize = 5;

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// Each sample repeats the work until it has run at least this long.
const SAMPLE: Duration = Duration::from_millis(20);

/// Median time per run of `command`, natively.
fn native(command: &str) -> (Duration, String) {
    let mut out = String::new();
    let mut reps = 1u32;
    loop {
        let t = Instant::now();
        for _ in 0..reps {
            out = black_box(s8_kernels::run(black_box(command)));
        }
        if t.elapsed() >= SAMPLE || reps >= 1 << 24 {
            break;
        }
        reps *= 2;
    }
    let times = (0..RUNS)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..reps {
                black_box(s8_kernels::run(black_box(command)));
            }
            t.elapsed() / reps
        })
        .collect();
    (median(times), out)
}

/// Median time per call of `command` into the instance, round trip.
async fn guest(instance: &InstanceHandle, command: &str) -> (Duration, String) {
    let call = || async {
        let reply = instance
            .call(Envelope::new("text/plain", command), Duration::from_secs(600))
            .await
            .unwrap();
        String::from_utf8(reply.body).unwrap()
    };
    let mut out = String::new();
    let mut reps = 1u32;
    loop {
        let t = Instant::now();
        for _ in 0..reps {
            out = call().await;
        }
        if t.elapsed() >= SAMPLE || reps >= 1 << 24 {
            break;
        }
        reps *= 2;
    }
    let mut times = Vec::new();
    for _ in 0..RUNS {
        let t = Instant::now();
        for _ in 0..reps {
            call().await;
        }
        times.push(t.elapsed() / reps);
    }
    (median(times), out)
}

fn fmt(d: Duration) -> String {
    let ns = d.as_secs_f64() * 1e9;
    if ns < 1e3 {
        format!("{ns:.0} ns")
    } else if ns < 1e6 {
        format!("{:.2} µs", ns / 1e3)
    } else if ns < 1e9 {
        format!("{:.2} ms", ns / 1e6)
    } else {
        format!("{:.2} s", ns / 1e9)
    }
}

async fn compare(instance: &InstanceHandle, label: &str, command: &str, per: u32) {
    let (n, n_out) = native(command);
    let (g, g_out) = guest(instance, command).await;
    assert_eq!(n_out, g_out, "{label}: native and guest disagree");
    let per = f64::from(per);
    println!(
        "| {label} | {} | {} | {:.2}x |",
        fmt(n.div_f64(per)),
        fmt(g.div_f64(per)),
        g.as_secs_f64() / n.as_secs_f64()
    );
}

async fn run(wasm: &str) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 8,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&std::fs::read(wasm).unwrap()).unwrap();
    let instance = rt
        .spawn(
            &app,
            InstanceSpec {
                app: "s8".into(),
                version: "0.0.0".into(),
                memory_max: 64 << 20,
                table_max: None,
                callback_deadline: Duration::from_secs(600),
                mailbox_max: 16,
                message_max_bytes: 1 << 20,
                registered_names: vec![],
                init_timeout: Duration::from_secs(10),
                shutdown_timeout: Duration::from_secs(1),
                init_args: vec![],
            },
        )
        .unwrap();

    // Call overhead: an empty call, round trip through the mailbox.
    let n = 20_000u32;
    for _ in 0..1_000 {
        instance.call(Envelope::new("text/plain", "noop"), Duration::from_secs(5)).await.unwrap();
    }
    let t = Instant::now();
    for _ in 0..n {
        instance.call(Envelope::new("text/plain", "noop"), Duration::from_secs(5)).await.unwrap();
    }
    let call = t.elapsed() / n;
    let t = Instant::now();
    for _ in 0..n {
        black_box(s8_kernels::run(black_box("noop")));
    }
    let native_call = t.elapsed() / n;
    println!("Empty call: native {} / Paddock call {}", fmt(native_call), fmt(call));
    println!();
    println!("| Workload | Native | In Paddock | Ratio |");
    println!("| --- | --- | --- | --- |");

    // Per-hash cost including one call per hash: overhead dominates small inputs.
    compare(&instance, "sha256 64 B, 1 per call", "sha256:64:1", 1).await;
    compare(&instance, "sha256 1 KiB, 1 per call", "sha256:1024:1", 1).await;
    // Many hashes per call: compute only, call overhead amortized.
    compare(&instance, "sha256 64 B, 100k per call", "sha256:64:100000", 100_000).await;
    compare(&instance, "sha256 1 MiB, 50 per call", "sha256:1048576:50", 50).await;
    compare(&instance, "n-body 5M steps", "nbody:5000000", 1).await;
    compare(&instance, "fannkuch-redux n=10", "fannkuch:10", 1).await;
}

fn main() {
    let mut args = std::env::args().skip(1);
    let wasm = args.next().expect("usage: s8-bench <s8_guest.wasm> [workers]");
    let workers = args.next().and_then(|w| w.parse().ok()).unwrap_or(2);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(async move { run(&wasm).await }).await.unwrap() });
}
