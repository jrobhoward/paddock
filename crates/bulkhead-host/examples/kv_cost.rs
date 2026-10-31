//! Keyvalue buckets against ETS, for `docs/BASELINE.md`: the loops of
//! `bulkhead-fixture-kvbench`, run inside a guest against memory buckets,
//! as `scripts/beam_ets.escript` runs them inside an Erlang process
//! against ETS tables. The same keys (`k000000` ...), value sizes and
//! pseudo-random key sequence on both sides.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p bulkhead-fixture-kvbench \
//!     --target-dir target/fixtures
//! cargo run --release -p bulkhead-host --example kv_cost -- [workers]
//! ```
//!
//! `KV_READ_CONCURRENCY=1` makes every bucket a `read_concurrency` one
//! (reads without a lock, spike S26).
//!
//! `KV_ONLY=<op>:<bucket>:<keys>:<size>:<seconds>` instead runs one loop
//! (after filling the bucket) over and over for that long, for a profiler:
//! `KV_ONLY=miss:b1k:1000:16:20 perf record -g target/release/examples/kv_cost 1`.
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
//! - Batches and dependent reads: `get_many` of 10 and 100 keys, reads
//!   along a chain whose values name the next key, and 1,000 reads per call
//!   timed by the host, so the call that starts them counts.
//! - Readers beside a writer: 1 to 16 instances reading one shared bucket
//!   while one more overwrites it as fast as it can.
//! - Memory, measured first: the host's resident memory before and after
//!   filling a bucket with 100,000 keys.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{AccessMode, Bucket, Capability, Durability};
use bulkhead_host::{
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
        "/../../target/fixtures/wasm32-wasip2/release/bulkhead_fixture_kvbench.wasm"
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

/// `KV_READ_CONCURRENCY=1`: every bucket reads without a lock.
fn read_concurrency() -> bool {
    std::env::var_os("KV_READ_CONCURRENCY").is_some_and(|v| v == "1")
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
        read_concurrency: read_concurrency(),
    };
    s.services.keyvalue.buckets = ["b1k", "b100k", "m16", "m1k", "chain"]
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
                read_concurrency: read_concurrency(),
            },
        )]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app: LoadedApp = rt.load(&component).unwrap();
    let h = rt.spawn(&app, spec()).unwrap();
    if let Ok(only) = std::env::var("KV_ONLY") {
        return only_loop(&h, &only).await;
    }
    println!(
        "tokio workers: {workers}; median of {RUNS} runs; ns per operation, timed in the guest\n"
    );
    // First, before other buckets free memory the allocator could reuse.
    memory(&h).await;
    single(&h).await;
    ordered(&h).await;
    dependent(&h).await;
    concurrency(&rt, &app, &h).await;
}

/// One loop, over and over, for a profiler; prints ns per operation.
async fn only_loop(h: &InstanceHandle, spec: &str) {
    let f: Vec<&str> = spec.split(':').collect();
    let [op, bucket, keys, size, seconds] = f[..] else {
        panic!("KV_ONLY=<op>:<bucket>:<keys>:<size>:<seconds>, not {spec}");
    };
    ask(h, &format!("fill:{bucket}:k:{keys}:{size}")).await;
    let until = Instant::now() + Duration::from_secs(seconds.parse().unwrap());
    let (mut ops, mut ns) = (0u64, 0f64);
    let n = 100_000;
    while Instant::now() < until {
        ns += ask(h, &format!("loop:{bucket}:{op}:{n}:{size}:{keys}:{ops}"))
            .await
            .parse::<f64>()
            .unwrap();
        ops += n;
    }
    println!(
        "{op} {bucket} {keys} keys {size} B: {:.0} ns/op over {ops} ops",
        ns / ops as f64
    );
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
            let ops: &[&str] = if size == 65536 {
                &["get", "miss", "set", "insert", "delete", "incr"]
            } else {
                &[
                    "get", "miss", "set", "insert", "delete", "incr", "many10", "many100",
                ]
            };
            for &op in ops {
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

/// Dependent reads along a chain, and 1,000 reads per call timed by the
/// host.
async fn dependent(h: &InstanceHandle) {
    println!("\n{:<14} {:>8} {:>10}", "reads", "keys", "ns/read");
    for keys in [1_000u64, 100_000] {
        ask(h, &format!("fillchain:chain:{keys}")).await;
        let ns = per_op(h, "chain", "chain", 100_000, 0, keys).await;
        println!("{:<14} {keys:>8} {ns:>10.0}", "chain");
    }
    let mut per_read = Vec::with_capacity(200);
    for seed in 0..200 {
        let started = Instant::now();
        ask(h, &format!("loop:b100k:get:1000:16:100000:{seed}")).await;
        per_read.push(started.elapsed().as_secs_f64() * 1e9 / 1000.0);
    }
    per_read.sort_by(f64::total_cmp);
    println!(
        "{:<14} {:>8} {:>10.0}",
        "1000 per call", 100_000, per_read[100]
    );
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
    readers_and_writer(rt, app, &pool, shared_keys).await;
}

/// Readers of the shared bucket while one more instance overwrites it as
/// fast as it can: the readers' operations per second, and the writer's.
async fn readers_and_writer(rt: &Runtime, app: &LoadedApp, pool: &[InstanceHandle], keys: u64) {
    println!(
        "\n{:<8} {:>9} {:>14} {:>14}",
        "readers", "instances", "reads/s", "writes/s"
    );
    let writer = rt.spawn(app, spec()).unwrap();
    ask(&writer, "loop:shared:get:1:16:1:0").await;
    let n = 50_000u64;
    for n_inst in [1usize, 4, 16] {
        let started = Instant::now();
        let w = writer.clone();
        let writes = tokio::spawn(async move {
            let ns: f64 = ask(&w, &format!("loop:shared:set:{n}:16:{keys}:99"))
                .await
                .parse()
                .unwrap();
            n as f64 / (ns / 1e9)
        });
        futures_join(&pool[..n_inst], |i| {
            format!("loop:shared:get:{n}:16:{keys}:{i}")
        })
        .await;
        let reads = n as f64 * n_inst as f64 / started.elapsed().as_secs_f64();
        let writes = writes.await.unwrap();
        println!("{:<8} {n_inst:>9} {reads:>14.0} {writes:>14.0}", "get+set");
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
