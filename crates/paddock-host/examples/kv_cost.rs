//! Keyvalue buckets against ETS, for `docs/BASELINE.md`: the loops of
//! `paddock-fixture-kvbench`, run inside a guest against memory buckets,
//! as `scripts/beam_ets.escript` runs them inside an Erlang process
//! against ETS tables. The same keys (`k000000` ...), value sizes and
//! pseudo-random key sequence on both sides.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-kvbench \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock-host --example kv_cost -- [workers]
//! ```
//!
//! Each figure is the median of 7 runs of a loop timed by the guest's own
//! clock, so it is what an app's code pays per operation: the host call,
//! the copies across the sandbox, the lock, the revision and quota
//! bookkeeping. Not included: the call that starts the loop.
//! - Single operations, in buckets of 1,000 and 100,000 keys, values of 16
//!   B, 1 KiB and 64 KiB (64 KiB with 1,000 keys only).
//! - Ordered reads: a 100-entry range from a random key, a scan of the
//!   whole bucket, and `next` one key at a time.
//! - Concurrency: 1 to 16 instances on one shared bucket of 100,000 keys
//!   (16 B), 95 or 50 reads in 100, or every write to one key; operations
//!   per second across all of them, timed by the host.
//! - Memory, measured first: the host's resident memory before and after
//!   filling a bucket with 100,000 keys.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use paddock_bundle::manifest::{AccessMode, Bucket, Capability, Durability};
use paddock_host::{
    Envelope, InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig, SharedBucket,
};

const RUNS: usize = 7;
const LONG: Duration = Duration::from_secs(600);

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |a| a.parse().expect("workers"));
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/fixtures/wasm32-wasip2/release/paddock_fixture_kvbench.wasm"
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

async fn ask(h: &InstanceHandle, body: &str) -> String {
    let reply = h
        .call(Envelope::new("text/plain", body), LONG)
        .await
        .unwrap();
    let text = String::from_utf8(reply.body).unwrap();
    assert!(!text.starts_with("err:"), "{body}: {text}");
    text
}

fn spec() -> InstanceSpec {
    let mut s = InstanceSpec {
        memory_max: 256 << 20,
        callback_deadline: LONG,
        granted: [Capability::KeyValue].into(),
        ..InstanceSpec::new("kvbench", "1.0.0")
    };
    let memory = || Bucket {
        durability: Durability::Memory,
        quota: None,
    };
    s.services.keyvalue.buckets = ["b1k", "b100k", "m16", "m1k"]
        .iter()
        .map(|n| ((*n).to_string(), memory()))
        .collect();
    s.services.keyvalue.shared = BTreeMap::from([("shared".to_string(), AccessMode::Rw)]);
    s
}

/// Median nanoseconds per operation of `RUNS` runs of a loop.
async fn per_op(h: &InstanceHandle, bucket: &str, op: &str, n: u64, size: u64, keys: u64) -> f64 {
    let mut runs = Vec::with_capacity(RUNS);
    for seed in 0..RUNS {
        let ns: f64 = ask(h, &format!("loop:{bucket}:{op}:{n}:{size}:{keys}:{seed}"))
            .await
            .parse()
            .unwrap();
        runs.push(ns / n as f64);
    }
    runs.sort_by(f64::total_cmp);
    runs[RUNS / 2]
}

fn resident_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    // 4 KiB pages on both machines in BASELINE.md.
    pages * 4096
}

async fn run(workers: usize, component: Vec<u8>) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 32,
        buckets: BTreeMap::from([(
            "shared".to_string(),
            SharedBucket {
                durability: Durability::Memory,
                quota: None,
            },
        )]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app: LoadedApp = rt.load(&component).unwrap();
    let h = rt.spawn(&app, spec()).unwrap();
    println!(
        "tokio workers: {workers}; median of {RUNS} runs; ns per operation, timed in the guest\n"
    );
    // First, before other buckets free memory the allocator could reuse.
    memory(&h).await;
    single(&h).await;
    ordered(&h).await;
    concurrency(&rt, &app, &h).await;
}

/// Resident bytes per entry, 100,000 entries of 16 B and of 1 KiB.
async fn memory(h: &InstanceHandle) {
    println!("{:<10} {:>8} {:>14}", "memory", "value", "bytes/entry");
    for (bucket, size) in [("m16", 16u64), ("m1k", 1024)] {
        let before = resident_bytes();
        ask(h, &format!("fill:{bucket}:k:100000:{size}")).await;
        let after = resident_bytes();
        println!(
            "{:<10} {size:>8} {:>14.0}",
            "resident",
            (after.saturating_sub(before)) as f64 / 100_000.0
        );
    }
}

/// Single operations by bucket size and value size.
async fn single(h: &InstanceHandle) {
    println!(
        "\n{:<10} {:>8} {:>8} {:>10}",
        "op", "keys", "value", "ns/op"
    );
    for (keys, bucket) in [(1_000u64, "b1k"), (100_000, "b100k")] {
        for size in [16u64, 1024, 65536] {
            if size == 65536 && keys > 1_000 {
                continue;
            }
            ask(h, &format!("fill:{bucket}:k:{keys}:{size}")).await;
            let n = if size == 65536 { 5_000 } else { 100_000 };
            for op in ["get", "miss", "set", "insert", "delete", "incr"] {
                let ns = per_op(h, bucket, op, n, size, keys).await;
                println!("{op:<10} {keys:>8} {size:>8} {ns:>10.0}");
            }
        }
        // Back to 16 B values for the ordered reads.
        ask(h, &format!("fill:{bucket}:k:{keys}:16")).await;
    }
}

/// Ordered reads: a 100-entry range, the whole bucket, one key at a time.
async fn ordered(h: &InstanceHandle) {
    println!(
        "\n{:<10} {:>8} {:>12} {:>12}",
        "ordered", "keys", "ns/call", "ns/entry"
    );
    for (keys, bucket) in [(1_000u64, "b1k"), (100_000, "b100k")] {
        let range = per_op(h, bucket, "range", 2_000, 16, keys).await;
        println!(
            "{:<10} {keys:>8} {range:>12.0} {:>12.1}",
            "range",
            range / 100.0
        );
        let scans = if keys > 1_000 { 5 } else { 200 };
        let all = per_op(h, bucket, "all", scans, 16, keys).await;
        println!(
            "{:<10} {keys:>8} {all:>12.0} {:>12.1}",
            "all",
            all / keys as f64
        );
        let walk = per_op(h, bucket, "walk", 20_000, 16, keys).await;
        println!("{:<10} {keys:>8} {walk:>12.0} {walk:>12.1}", "next");
    }
}

/// 1, 4 and 16 instances on one shared bucket at once.
async fn concurrency(rt: &Runtime, app: &LoadedApp, h: &InstanceHandle) {
    println!(
        "\n{:<8} {:>9} {:>14} {:>12}",
        "mix", "instances", "ops/s", "ns/op each"
    );
    let shared_keys = 100_000u64;
    ask(h, &format!("fill:shared:k:{shared_keys}:16")).await;
    let pool: Vec<InstanceHandle> = (0..16).map(|_| rt.spawn(app, spec()).unwrap()).collect();
    for p in &pool {
        ask(p, "loop:shared:get:1:16:1:0").await;
    }
    for mix in ["mix95", "mix50", "hot"] {
        for n_inst in [1usize, 4, 16] {
            let n = 50_000u64;
            let started = Instant::now();
            let each: Vec<String> = futures_join(&pool[..n_inst], |i| {
                format!("loop:shared:{mix}:{n}:16:{shared_keys}:{i}")
            })
            .await;
            let wall = started.elapsed().as_secs_f64();
            let total = n as f64 * n_inst as f64;
            let mean_each: f64 = each.iter().map(|s| s.parse::<f64>().unwrap()).sum::<f64>()
                / each.len() as f64
                / n as f64;
            println!(
                "{mix:<8} {n_inst:>9} {:>14.0} {mean_each:>12.0}",
                total / wall
            );
        }
    }
}

/// Ask each instance in `pool` its command at once; the replies in order.
async fn futures_join(pool: &[InstanceHandle], command: impl Fn(usize) -> String) -> Vec<String> {
    let tasks: Vec<_> = pool
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let p = p.clone();
            let body = command(i);
            tokio::spawn(async move { ask(&p, &body).await })
        })
        .collect();
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}
