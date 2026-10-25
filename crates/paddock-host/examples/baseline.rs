//! Baseline numbers for `docs/BASELINE.md`: call latency, cast throughput,
//! instantiate time and idle memory per instance, through `paddock-host`.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock-host --example baseline -- \
//!     [workers] [instances] [standard|dense] [component.wasm]
//! ```
//!
//! Defaults: 1 tokio worker, 1,000 instances, the standard memory profile,
//! the chaos fixture under `target/fixtures`. Linux only for the memory figures (`/proc/self/statm`,
//! `/proc/self/smaps_rollup`). Resident memory counts a page shared by
//! several instances (the component's memory image) once for each; the
//! proportional figure (PSS) divides it among them, so it is what an
//! instance costs where the image is mapped in larger windows (aarch64).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::{Duration, Instant};

use paddock_host::{
    CallError, Envelope, InstanceHandle, InstanceSpec, LoadedApp, MemoryProfile, Runtime,
    RuntimeConfig,
};

const CALLS: usize = 100_000;
const CASTS: u64 = 200_000;
const LONG: Duration = Duration::from_secs(10);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let workers: usize = args.first().map_or(1, |a| a.parse().expect("workers"));
    let instances: usize = args.get(1).map_or(1_000, |a| a.parse().expect("instances"));
    let profile = match args.get(2).map(String::as_str) {
        None | Some("standard") => MemoryProfile::Standard,
        Some("dense") => MemoryProfile::Dense,
        Some(other) => panic!("unknown profile {other}"),
    };
    let path = args.get(3).cloned().unwrap_or_else(|| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/fixtures/wasm32-wasip2/release/paddock_fixture_chaos.wasm"
        )
        .to_string()
    });
    let component = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        // On a worker, as instance clients are: with one worker, a call and
        // its reply stay on the same thread.
        .block_on(async move { tokio::spawn(run(workers, instances, profile, component)).await })
        .unwrap();
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        memory_max: 32 << 20,
        mailbox_max: 1024,
        ..InstanceSpec::new("baseline", "0.1.0")
    }
}

fn text(body: &str) -> Envelope {
    Envelope::new("text/plain", body)
}

async fn run(workers: usize, instances: usize, profile: MemoryProfile, component: Vec<u8>) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: u32::try_from(instances + 8).unwrap(),
        memory_profile: profile,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let started = Instant::now();
    let app = rt.load(&component).unwrap();
    println!("tokio workers: {workers}, memory profile: {profile:?}");
    println!(
        "component: {} KiB, compiled in {:.0} ms",
        component.len() / 1024,
        started.elapsed().as_secs_f64() * 1e3
    );

    let probe = rt.spawn(&app, spec()).unwrap();
    probe.call(text("echo:warm"), LONG).await.unwrap();
    call_latency(&probe, "echo:x", "empty call").await;
    call_latency(&probe, &format!("echo:{}", "x".repeat(1024)), "1 KiB echo").await;
    cast_throughput(&probe).await;
    instantiate(&rt, &app, instances).await;
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
    format!("{:.2} µs", d.as_secs_f64() * 1e6)
}

async fn call_latency(instance: &InstanceHandle, body: &str, label: &str) {
    let mut samples = Vec::with_capacity(CALLS);
    let total = Instant::now();
    for _ in 0..CALLS {
        let t = Instant::now();
        instance.call(text(body), LONG).await.unwrap();
        samples.push(t.elapsed());
    }
    let total = total.elapsed();
    samples.sort();
    #[allow(clippy::cast_precision_loss)]
    let rate = CALLS as f64 / total.as_secs_f64();
    println!(
        "call, {label}: p50 {}, p99 {}, p99.9 {}, max {}; {rate:.0} calls/s sequential",
        us(quantile(&samples, 0.5)),
        us(quantile(&samples, 0.99)),
        us(quantile(&samples, 0.999)),
        us(*samples.last().unwrap()),
    );
}

/// Call `count`, retrying while the mailbox is full.
async fn count(instance: &InstanceHandle) -> u64 {
    loop {
        match instance.call(text("count"), LONG).await {
            Ok(reply) => return String::from_utf8(reply.body).unwrap().parse().unwrap(),
            Err(CallError::Overloaded) => tokio::task::yield_now().await,
            Err(e) => panic!("{e}"),
        }
    }
}

async fn cast_throughput(instance: &InstanceHandle) {
    let before = count(instance).await;
    let mut full = 0u64;
    let started = Instant::now();
    let mut sent = 0;
    while sent < CASTS {
        match instance.cast(text("count")) {
            Ok(()) => sent += 1,
            Err(CallError::Overloaded) => {
                full += 1;
                tokio::task::yield_now().await;
            }
            Err(e) => panic!("{e}"),
        }
    }
    // A call queues behind every cast, so its reply means all were handled.
    let after = count(instance).await;
    let elapsed = started.elapsed();
    assert_eq!(after, before + CASTS + 1, "every cast handled once");
    #[allow(clippy::cast_precision_loss)]
    let rate = CASTS as f64 / elapsed.as_secs_f64();
    println!(
        "cast: {rate:.0} casts/s handled ({CASTS} casts, mailbox 1024, sender saw a full mailbox {full} times)"
    );
}

/// Resident and virtual size of this process, in KiB.
fn memory() -> Option<(u64, u64)> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let mut fields = statm.split_whitespace().map(|f| f.parse::<u64>().ok());
    let (virt, rss) = (fields.next()??, fields.next()??);
    Some((rss * 4, virt * 4))
}

/// Proportional set size of this process, in KiB.
fn pss() -> Option<u64> {
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    rollup
        .lines()
        .find_map(|l| l.strip_prefix("Pss:"))?
        .trim()
        .trim_end_matches(" kB")
        .parse()
        .ok()
}

async fn instantiate(rt: &Runtime, app: &LoadedApp, n: usize) {
    let before = memory();
    let pss_before = pss();
    let mut handles = Vec::with_capacity(n);
    let mut samples = Vec::with_capacity(n);
    let started = Instant::now();
    for _ in 0..n {
        let t = Instant::now();
        let h = rt.spawn(app, spec()).unwrap();
        // The first reply comes after `init`, so this is spawn to ready.
        h.call(text("echo:up"), LONG).await.unwrap();
        samples.push(t.elapsed());
        handles.push(h);
    }
    let total = started.elapsed();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = memory();
    let pss_after = pss();
    samples.sort();
    println!(
        "instantiate to first reply ({n} instances): p50 {}, p99 {}, mean {}",
        us(quantile(&samples, 0.5)),
        us(quantile(&samples, 0.99)),
        us(total / u32::try_from(n).unwrap()),
    );
    if let (Some((rss0, virt0)), Some((rss1, virt1))) = (before, after) {
        let n = n as u64;
        println!(
            "idle memory per instance: {} KiB resident, {} KiB virtual (process: {} MiB resident, {} GiB virtual)",
            rss1.saturating_sub(rss0) / n,
            virt1.saturating_sub(virt0) / n,
            rss1 / 1024,
            virt1 / 1024 / 1024,
        );
    }
    if let (Some(p0), Some(p1)) = (pss_before, pss_after) {
        println!(
            "idle memory per instance, proportional: {} KiB (shared pages divided among their users)",
            p1.saturating_sub(p0) / n as u64,
        );
    }
    for h in &handles {
        h.kill();
    }
}
