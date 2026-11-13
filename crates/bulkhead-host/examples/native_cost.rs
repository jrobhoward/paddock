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
//!
//! `NATIVE_COST_SENDERS=N` has every instance hold `N` idle sender
//! channels (ROADMAP AP1) through the run.
//!
//! Under the privileged helper (ROADMAP PS1): start it as the helper's
//! node (its configuration's `node` this example, `node_args` the worker
//! count, `binds` a directory holding copies of `bulkhead-runner` and
//! `bulkhead-fixture-native` that `BULKHEAD_BENCH_DIR` names, and the
//! helper's `runner` the copy). The instances are then the helper's
//! children, not this process's, so the CPU per call counts the node's
//! time only.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{AccessMode, Bucket, Capability, Durability, NativeProgram};
use bulkhead_host::{
    Envelope, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime,
    RuntimeConfig, SharedBucket,
};

const LONG: Duration = Duration::from_secs(30);
const INSTANCES: usize = 100;

fn release(name: &str) -> PathBuf {
    match std::env::var_os("BULKHEAD_BENCH_DIR") {
        Some(dir) => PathBuf::from(dir).join(name),
        None => {
            PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/release")).join(name)
        }
    }
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
    spec.services.keyvalue.shared = [("published".to_string(), AccessMode::Rw)].into();
    spec
}

/// A bucket get from a callback, as a request and from a published
/// snapshot (T4).
async fn bucket_gets(pool: &[InstanceHandle]) {
    ask(&pool[0], "kv set data t 0123456789abcdef").await;
    println!(
        "bucket get from a callback (16 B, memory bucket): {:.1} µs (median of 5 runs of 20,000)",
        gets(&pool[0], "data").await / 1e3
    );
    // Written by another instance: the writer's own reads go to the node
    // until a snapshot holds its write.
    ask(&pool[1], "kv set published t 0123456789abcdef").await;
    // Mapped, and holding the key: the first snapshot may be from before
    // the write.
    while ask(&pool[0], "published published").await == "none"
        || ask(&pool[0], "kv get published t").await != "0123456789abcdef"
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    println!(
        "bucket get from a published snapshot (16 B, T4): {:.3} µs (median of 5 runs of 20,000)",
        gets(&pool[0], "published").await / 1e3
    );
}

/// Requests for the instance's own pid from a callback.
async fn callback_requests(h: &InstanceHandle) {
    let mut per = Vec::new();
    for _ in 0..5 {
        let ns: f64 = ask(h, "selfs 20000").await.parse().unwrap();
        per.push(ns / 20_000.0);
    }
    per.sort_by(f64::total_cmp);
    println!(
        "request from a callback (its own pid): {:.1} µs (median of 5 runs of 20,000)",
        per[2] / 1e3
    );
}

/// Idle sender channels held by every instance for the whole run
/// (`NATIVE_COST_SENDERS`, ROADMAP AP1): what watching more rings costs
/// the node, if anything.
async fn open_idle_senders(pool: &[InstanceHandle]) {
    let idle: usize = std::env::var("NATIVE_COST_SENDERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if idle == 0 {
        return;
    }
    for (i, h) in pool.iter().enumerate() {
        // The first instance keeps a place for the sender its rows time.
        for _ in 0..if i == 0 { idle.saturating_sub(1) } else { idle } {
            let n = ask(h, "sender open").await;
            assert!(!n.starts_with("err:"), "sender open: {n}");
        }
    }
    println!("{idle} idle sender channels per instance");
}

/// A request and a bucket get from a thread of the app's own, through a
/// sender channel (ROADMAP AP1), against the callback's above.
async fn sender_requests(h: &InstanceHandle) {
    let sender = ask(h, "sender open").await;
    for (what, command) in [
        (
            "request from a sender thread (its own pid)",
            format!("sender {sender} selfs 20000"),
        ),
        (
            "bucket get from a sender thread (16 B, memory bucket)",
            format!("sender {sender} gets data 20000"),
        ),
    ] {
        let mut per = Vec::new();
        for _ in 0..5 {
            let ns: f64 = ask(h, &command).await.parse().unwrap();
            per.push(ns / 20_000.0);
        }
        per.sort_by(f64::total_cmp);
        println!(
            "{what}: {:.1} µs (median of 5 runs of 20,000)",
            per[2] / 1e3
        );
    }
    ask(h, &format!("sender drop {sender}")).await;
}

/// Nanoseconds per get of key `t` from `bucket` in a callback: the
/// median of 5 runs of 20,000.
async fn gets(h: &InstanceHandle, bucket: &str) -> f64 {
    let mut per = Vec::new();
    for _ in 0..5 {
        let ns: f64 = ask(h, &format!("kv gets {bucket} 20000"))
            .await
            .parse()
            .unwrap();
        per.push(ns / 20_000.0);
    }
    per.sort_by(f64::total_cmp);
    per[2]
}

async fn run(workers: usize) {
    let mut native = NativeConfig::default();
    native.runner = Some(release("bulkhead-runner"));
    native.helper = bulkhead_host::PrivHelper::inherited().expect("the helper's socket");
    if native.helper.is_some() {
        println!("under the privileged helper: CPU per call is the node's alone");
    }
    // `NATIVE_SPIN_US`: the node's `[native] spin_us` (T1); unset, 20.
    if let Some(us) = std::env::var("NATIVE_SPIN_US")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        native.spin = Duration::from_micros(us);
    }
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 256,
        native,
        // A published bucket (T4), its snapshot read in the app.
        buckets: [(
            "published".to_string(),
            SharedBucket {
                durability: Durability::Memory,
                quota: None,
                read_concurrency: false,
                publish: Some(Duration::from_millis(10)),
            },
        )]
        .into(),
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
    open_idle_senders(&pool).await;

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

    callback_requests(&pool[0]).await;
    bucket_gets(&pool).await;
    sender_requests(&pool[0]).await;

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
