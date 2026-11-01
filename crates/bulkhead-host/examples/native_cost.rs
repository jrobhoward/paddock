//! Native-process instances for `docs/BASELINE.md`: calls from the host
//! and between instances, requests from inside a callback, casts, and
//! start time, through `bulkhead-runner` and `bulkhead-fixture-native`.
//!
//! ```sh
//! cargo build --release -p bulkhead-native --bin bulkhead-runner
//! cargo build --release -p bulkhead-fixture-native
//! cargo run --release -p bulkhead-host --example native_cost -- [workers]
//! ```
//!
//! Linux only. CPU per call is the node's and the app processes' user and
//! system time over the run, from `/proc`, divided by the calls.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{Bucket, Capability, Durability, NativeProgram};
use bulkhead_host::{
    Envelope, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime,
    RuntimeConfig,
};

const LONG: Duration = Duration::from_secs(30);
const INSTANCES: usize = 100;

fn release(name: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/release")).join(name)
}

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |a| a.parse().expect("workers"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(run(workers)).await })
        .unwrap();
}

async fn ask(h: &InstanceHandle, body: &str) -> String {
    let reply = h
        .call(Envelope::new("text/plain", body), LONG)
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

fn percentiles(mut v: Vec<Duration>) -> String {
    v.sort();
    let at = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    format!("p50 {:.1?}, p99 {:.1?}", at(0.5), at(0.99))
}

/// User and system seconds of this process and its children still
/// running (the app processes).
fn cpu_seconds() -> f64 {
    let me = std::process::id();
    let ticks = |pid: &str| -> Option<(u32, f64)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let f: Vec<&str> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
        let t = f[11].parse::<f64>().ok()? + f[12].parse::<f64>().ok()?;
        Some((f[1].parse().ok()?, t / 100.0))
    };
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|p| {
            let (parent, t) = ticks(&p)?;
            (p == me.to_string() || parent == me).then_some(t)
        })
        .sum()
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) % below as u64) as usize
    }
}

fn spec() -> InstanceSpec {
    let mut spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: LONG,
        init_timeout: LONG,
        // Room for the cast burst below.
        mailbox_max: 100_000,
        granted: [Capability::KeyValue].into(),
        ..InstanceSpec::new("native", "1.0.0")
    };
    spec.services.keyvalue.buckets = [(
        "data".to_string(),
        Bucket {
            durability: Durability::Memory,
            quota: None,
            read_concurrency: false,
        },
    )]
    .into();
    spec
}

async fn run(workers: usize) {
    let mut native = NativeConfig::default();
    native.runner = Some(release("bulkhead-runner"));
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 256,
        native,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app: LoadedApp = rt.load_native(NativeCode {
        path: release("bulkhead-fixture-native"),
        kind: NativeProgram::Executable,
    });
    println!("tokio workers: {workers}; {INSTANCES} native instances\n");

    let mut starts = Vec::new();
    let mut pool = Vec::new();
    for _ in 0..INSTANCES {
        let t = Instant::now();
        let h = rt.spawn(&app, spec()).unwrap();
        ask(&h, "echo x").await;
        starts.push(t.elapsed());
        pool.push(h);
    }
    println!("start, to the first reply: {}", percentiles(starts));

    let mut rng = Lcg(7);
    let mut one = Vec::new();
    for _ in 0..20_000 {
        let h = &pool[rng.next(INSTANCES)];
        let t = Instant::now();
        ask(h, "echo x").await;
        one.push(t.elapsed());
    }
    println!("call from the host, one at a time: {}", percentiles(one));

    let mut relay = Vec::new();
    for _ in 0..10_000 {
        let i = rng.next(INSTANCES);
        let to = pool[(i + 1 + rng.next(INSTANCES - 1)) % INSTANCES].pid();
        let t = Instant::now();
        let r = ask(
            &pool[i],
            &format!("call {}.{}.{} echo x", to.node, to.id, to.incarnation),
        )
        .await;
        assert_eq!(r, "x");
        relay.push(t.elapsed());
    }
    println!(
        "call between instances (through the host): {}",
        percentiles(relay)
    );

    let mut per = Vec::new();
    for _ in 0..5 {
        let ns: f64 = ask(&pool[0], "selfs 20000").await.parse().unwrap();
        per.push(ns / 20_000.0);
    }
    per.sort_by(f64::total_cmp);
    println!(
        "request from a callback (its own pid): {:.1} µs (median of 5 runs of 20,000)",
        per[2] / 1e3
    );

    ask(&pool[0], "kv set data t 0123456789abcdef").await;
    let mut per = Vec::new();
    for _ in 0..5 {
        let ns: f64 = ask(&pool[0], "kv gets data 20000").await.parse().unwrap();
        per.push(ns / 20_000.0);
    }
    per.sort_by(f64::total_cmp);
    println!(
        "bucket get from a callback (16 B, memory bucket): {:.1} µs (median of 5 runs of 20,000)",
        per[2] / 1e3
    );

    let casts = 50_000;
    let t = Instant::now();
    for _ in 0..casts {
        pool[1].cast(Envelope::new("text/plain", "x")).unwrap();
    }
    ask(&pool[1], "echo done").await;
    println!(
        "casts to one instance: {:.0} handled per second",
        f64::from(casts) / t.elapsed().as_secs_f64()
    );

    for in_flight in [16usize, 64] {
        let cpu0 = cpu_seconds();
        let t = Instant::now();
        let tasks: Vec<_> = (0..in_flight)
            .map(|k| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    let mut rng = Lcg(k as u64);
                    let mut n = 0u64;
                    let until = Instant::now() + Duration::from_secs(3);
                    while Instant::now() < until {
                        ask(&pool[rng.next(INSTANCES)], "echo x").await;
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        let mut total = 0;
        for task in tasks {
            total += task.await.unwrap();
        }
        let wall = t.elapsed().as_secs_f64();
        let per_s = total as f64 / wall;
        let cores = (cpu_seconds() - cpu0) / wall;
        println!(
            "{in_flight} calls in flight: {per_s:.0} calls/s, {cores:.2} cores, {:.1} µs of CPU per call",
            cores * 1e6 / per_s
        );
    }
    for h in &pool {
        h.kill();
    }
}
