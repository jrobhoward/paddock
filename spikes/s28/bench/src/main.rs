//! Spike S28: a mirror of a bucket in each instance, kept by a scan and a
//! watch, against reads through the bucket and through owner instances.
//!
//! s28-bench <guest.wasm> [phase ...]
//!
//! Phases (all by default): `warm` (time to build a mirror), `memory` (PSS
//! per mirror at 1, 16 and 100 instances), `read` (one reader, then 1, 4
//! and 16), `lag` (readers beside a writer at 1,000 and 100,000 writes per
//! second), `writer` (a writer's cost with 0 to 100 mirrors watching),
//! `resume` (building a mirror by scan-then-resume while another instance
//! deletes keys). `S28_RC=0` makes the buckets plain memory buckets
//! (`read_concurrency` off).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bulkhead_bundle::manifest::{Bucket, Capability, Durability};
use bulkhead_host::{Envelope, InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

const LONG: Duration = Duration::from_secs(600);
const OWNERS: u64 = 4;
const KEYS: u64 = 100_000;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let phases: Vec<&str> = if args.len() > 1 {
        args[1..].iter().map(String::as_str).collect()
    } else {
        vec!["warm", "memory", "read", "lag", "writer", "resume"]
    };
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 160,
        // Freed slots give their pages back, so each row's PSS is its own.
        keep_resident: Some(0),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let component = std::fs::read(&args[0]).unwrap();
    let app = rt.load(&component).unwrap();
    let rc = std::env::var("S28_RC").map_or(true, |v| v != "0");
    println!(
        "4 workers; buckets in memory, read_concurrency {}",
        if rc { "on" } else { "off" }
    );
    let b = Bench { rt, app, rc };
    let filler = b.spawn();
    for (bucket, n, size) in [
        ("b1k", 1_000u64, 16u64),
        ("b100k", KEYS, 16),
        ("b100k1k", KEYS, 1024),
        ("live", KEYS, 16),
    ] {
        ask(&filler, &format!("fill:{bucket}:{n}:{size}")).await;
    }
    ask(&filler, &format!("fillchain:chain:{KEYS}")).await;
    filler.kill();
    for phase in phases {
        match phase {
            "warm" => b.warm().await,
            "memory" => b.memory().await,
            "read" => b.read().await,
            "lag" => b.lag().await,
            "writer" => b.writer().await,
            "resume" => b.resume().await,
            other => panic!("unknown phase {other}"),
        }
    }
}

struct Bench {
    rt: Runtime,
    app: LoadedApp,
    rc: bool,
}

async fn ask_raw(h: &InstanceHandle, body: &str) -> String {
    let reply = h
        .call(Envelope::new("text/plain", body), LONG)
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

async fn ask(h: &InstanceHandle, body: &str) -> String {
    let text = ask_raw(h, body).await;
    assert!(!text.starts_with("err:"), "{body}: {text}");
    text
}

fn fields(s: &str) -> Vec<u64> {
    s.split_whitespace().map(|f| f.parse().unwrap()).collect()
}

fn wall_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Ask each instance its command at once; the replies in order, and the
/// wall time.
async fn all(pool: &[InstanceHandle], command: impl Fn(usize) -> String) -> (Vec<String>, f64) {
    let started = Instant::now();
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
    (out, started.elapsed().as_secs_f64())
}

impl Bench {
    fn spec(&self) -> InstanceSpec {
        let mut s = InstanceSpec {
            memory_max: 512 << 20,
            callback_deadline: LONG,
            granted: [Capability::KeyValue].into(),
            registered_names: (0..OWNERS)
                .map(|i| format!("s28-owner-{i}"))
                .chain(["s28-holder".to_string()])
                .collect(),
            ..InstanceSpec::new("s28", "1.0.0")
        };
        s.services.keyvalue.buckets = ["b1k", "b100k", "b100k1k", "live", "chain"]
            .iter()
            .map(|n| {
                (
                    (*n).to_string(),
                    Bucket {
                        durability: Durability::Memory,
                        quota: None,
                        read_concurrency: self.rc,
                    },
                )
            })
            .collect();
        s
    }

    fn spawn(&self) -> InstanceHandle {
        self.rt.spawn(&self.app, self.spec()).unwrap()
    }

    /// `n` instances, each with a mirror; the mirror replies.
    async fn mirrors(
        &self,
        n: usize,
        bucket: &str,
        kind: &str,
    ) -> (Vec<InstanceHandle>, Vec<String>) {
        let pool: Vec<InstanceHandle> = (0..n).map(|_| self.spawn()).collect();
        let (replies, _) = all(&pool, |_| format!("mirror:{bucket}:k:watch:{kind}")).await;
        (pool, replies)
    }

    async fn warm(&self) {
        println!("\n== warm-up: building a mirror (median of 5 instances, one at a time)");
        for (bucket, label, mode, kind) in [
            ("b1k", "1,000 x 16 B", "watch", "hash"),
            ("b100k", "100,000 x 16 B", "watch", "hash"),
            ("b100k", "100,000 x 16 B", "watch", "fx"),
            ("b100k", "100,000 x 16 B", "watch", "tree"),
            ("b100k", "100,000 x 16 B", "resume", "hash"),
            ("b100k1k", "100,000 x 1 KiB", "watch", "hash"),
        ] {
            let mut times = Vec::new();
            let mut memory = 0;
            for _ in 0..5 {
                let h = self.spawn();
                let r = fields(&ask(&h, &format!("mirror:{bucket}:k:{mode}:{kind}")).await);
                times.push(r[1] as f64);
                memory = r[3];
                h.kill();
            }
            let t = median(times);
            println!(
                "  {label:<16} {mode:<6} {kind:<4}: {:.1} ms ({:.0} ns per entry); linear memory {} MiB",
                t / 1e6,
                t / bucket_keys(bucket) as f64,
                memory >> 20
            );
        }
    }

    async fn memory(&self) {
        println!(
            "\n== memory per mirror (PSS of the process; instances started first, then their mirrors)"
        );
        for (bucket, kind, counts) in [
            ("b100k", "hash", &[1usize, 16, 100][..]),
            ("b100k", "fx", &[1, 16][..]),
            ("b100k", "tree", &[1, 16][..]),
            ("b100k1k", "hash", &[1, 16][..]),
        ] {
            for &n in counts {
                let base = Mem::now();
                let pool: Vec<InstanceHandle> = (0..n).map(|_| self.spawn()).collect();
                all(&pool, |_| "mem".into()).await;
                let started = Mem::now();
                let (replies, _) = all(&pool, |_| format!("mirror:{bucket}:k:watch:{kind}")).await;
                let mirrored = Mem::now();
                let linear = fields(&replies[0])[3];
                println!(
                    "  {bucket:<8} {kind:<4} {n:>3} instances: started {:+.1} MiB each; mirrors {:+.1} MiB each (anon {:+.1}); linear memory {} MiB, charged {} MiB",
                    started.pss_mib(&base) / n as f64,
                    mirrored.pss_mib(&started) / n as f64,
                    mirrored.anon_mib(&started) / n as f64,
                    linear >> 20,
                    pool[0].memory_bytes() >> 20,
                );
                for h in &pool {
                    h.kill();
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    }

    async fn read(&self) {
        println!("\n== reads, 100,000 keys of 16 B (median of 5 runs, timed in the guest)");
        let (m, _) = self.mirrors(1, "b100k", "hash").await;
        let (t, _) = self.mirrors(1, "b100k", "tree").await;
        let (x, _) = self.mirrors(1, "b100k", "fx").await;
        let c = self.spawn();
        ask(&c, "mirror:chain:c:watch:hash").await;
        let owners: Vec<InstanceHandle> = (0..OWNERS).map(|_| self.spawn()).collect();
        for (i, o) in owners.iter().enumerate() {
            ask(o, &format!("owner:{i}:{OWNERS}:b100k")).await;
        }
        let n = 200_000;
        let per = |cmd: String, h: InstanceHandle, n: u64| async move {
            let mut v = Vec::new();
            for seed in 0..5 {
                let ns: f64 = ask(&h, &cmd.replace("SEED", &seed.to_string()))
                    .await
                    .parse()
                    .unwrap();
                v.push(ns / n as f64);
            }
            median(v)
        };
        let rows = [
            (
                "mirror, hash map",
                per(format!("mget:{n}:{KEYS}:SEED"), m[0].clone(), n).await,
            ),
            (
                "mirror, hash map, Fx",
                per(format!("mget:{n}:{KEYS}:SEED"), x[0].clone(), n).await,
            ),
            (
                "mirror, B-tree",
                per(format!("mget:{n}:{KEYS}:SEED"), t[0].clone(), n).await,
            ),
            (
                "bucket get",
                per(format!("bget:b100k:{n}:{KEYS}:SEED"), m[0].clone(), n).await,
            ),
            (
                "owner call (4 owners)",
                per(
                    format!("oget:{OWNERS}:20000:{KEYS}:SEED"),
                    m[0].clone(),
                    20_000,
                )
                .await,
            ),
            (
                "chain, mirror",
                per(format!("mchain:{n}:{KEYS}:SEED"), c.clone(), n).await,
            ),
            (
                "chain, bucket",
                per(format!("bchain:chain:{n}:{KEYS}:SEED"), c.clone(), n).await,
            ),
        ];
        for (label, ns) in rows {
            println!("  {label:<22} {ns:>8.0} ns");
        }
        println!("  reads per second from 1, 4 and 16 instances at once:");
        let (pool, _) = self.mirrors(16, "b100k", "hash").await;
        for (label, cmd, n) in [
            ("mirror, hash map", "mget:{n}:{KEYS}:{i}", 2_000_000u64),
            ("bucket get", "bget:b100k:{n}:{KEYS}:{i}", 200_000),
            ("owner call (4 owners)", "oget:4:{n}:{KEYS}:{i}", 20_000),
        ] {
            let mut line = format!("  {label:<22}");
            for k in [1usize, 4, 16] {
                let (_, wall) = all(&pool[..k], |i| {
                    cmd.replace("{n}", &n.to_string())
                        .replace("{KEYS}", &KEYS.to_string())
                        .replace("{i}", &i.to_string())
                })
                .await;
                line += &format!(" {:>8.2}M", (n * k as u64) as f64 / wall / 1e6);
            }
            println!("{line}");
        }
        for h in pool
            .iter()
            .chain(&m)
            .chain(&t)
            .chain(&x)
            .chain(&owners)
            .chain([&c])
        {
            h.kill();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    /// Wait for readers to stop receiving notices; the last apply's wall
    /// time across them.
    async fn drained(readers: &[InstanceHandle]) -> u64 {
        let mut last = Vec::new();
        let mut steady = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let (p, _) = all(readers, |_| "progress".into()).await;
            let now: Vec<Vec<u64>> = p.iter().map(|s| fields(s)).collect();
            if now == last {
                steady += 1;
                if steady == 2 {
                    return now.iter().map(|f| f[1]).max().unwrap_or(0);
                }
            } else {
                steady = 0;
            }
            last = now;
        }
    }

    async fn lag(&self) {
        println!(
            "\n== readers beside a writer: random overwrites of 100,000 keys for 5 s, every reader mirroring all of them"
        );
        println!(
            "  {:>7} {:>7} {:>9} {:>9} {:>8} {:>10} {:>10} {:>10} {:>9} {:>6} {:>5} {:>6}",
            "rate",
            "readers",
            "writes/s",
            "notices",
            "stale",
            "lag p50",
            "p99",
            "max",
            "drain",
            "cores",
            "sys",
            "differ"
        );
        // S28_LAG=<rate>:<readers>:<seconds> runs one row, for a profiler.
        let only: Option<Vec<u64>> = std::env::var("S28_LAG")
            .ok()
            .map(|v| v.split(':').map(|f| f.parse().unwrap()).collect());
        let rows: Vec<(u64, usize)> = match &only {
            Some(o) => vec![(o[0], o[1] as usize)],
            None => [1_000u64, 100_000]
                .iter()
                .flat_map(|&r| [0usize, 1, 4, 16].map(|n| (r, n)))
                .collect(),
        };
        let seconds = only.as_ref().map_or(5, |o| o[2]);
        let writer = self.spawn();
        for (rate, readers) in rows {
            {
                let (pool, _) = self.mirrors(readers, "live", "hash").await;
                all(&pool, |_| "stats".into()).await;
                let per_tick = rate / 1000;
                let cpu0 = cpu_seconds();
                let sys0 = sys_seconds();
                let started = Instant::now();
                let mut tick = tokio::time::interval(Duration::from_millis(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
                let mut writes = 0;
                while started.elapsed() < Duration::from_secs(seconds) {
                    tick.tick().await;
                    ask(&writer, &format!("write:live:{per_tick}:{KEYS}")).await;
                    writes += per_tick;
                }
                let stop = wall_ns();
                let wall = started.elapsed().as_secs_f64();
                let last = Self::drained(&pool).await;
                let cores = (cpu_seconds() - cpu0) / started.elapsed().as_secs_f64();
                let sys = (sys_seconds() - sys0) / started.elapsed().as_secs_f64();
                let (stats, _) = all(&pool, |_| "stats".into()).await;
                let (checks, _) = all(&pool, |_| "check:live:k".into()).await;
                let s: Vec<Vec<u64>> = stats.iter().map(|x| fields(x)).collect();
                let differ: u64 = checks
                    .iter()
                    .map(|c| c.split_whitespace().next().unwrap().parse::<u64>().unwrap())
                    .sum();
                for c in checks.iter().filter(|c| !c.starts_with("0")) {
                    println!("  differs: {c}");
                }
                let mean =
                    |i: usize| s.iter().map(|f| f[i] as f64).sum::<f64>() / s.len().max(1) as f64;
                let max = |i: usize| s.iter().map(|f| f[i]).max().unwrap_or(0);
                println!(
                    "  {rate:>7} {readers:>7} {:>9.0} {:>8.0}% {:>7.0}% {:>10} {:>10} {:>10} {:>9} {cores:>6.2} {sys:>5.2} {differ:>6}",
                    writes as f64 / wall,
                    100.0 * mean(1) / writes as f64,
                    100.0 * mean(2) / writes as f64,
                    us(mean(3)),
                    us(max(4) as f64),
                    us(max(5) as f64),
                    if readers == 0 {
                        "-".into()
                    } else {
                        us(last.saturating_sub(stop) as f64)
                    },
                );
                for h in &pool {
                    h.kill();
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        println!(
            "  notices and stale: per reader, as a share of the writes; lag p50 is the readers' mean, p99 and max the worst reader's; drain: from the writer's last write to the last notice applied"
        );
        writer.kill();
    }

    async fn writer(&self) {
        println!(
            "\n== a writer's cost: 100,000 overwrites as fast as it can, mirrors watching every key"
        );
        let writer = self.spawn();
        let holder = self.spawn();
        ask(&holder, "holder").await;
        for (watchers, held) in [
            (0usize, false),
            (1, false),
            (16, false),
            (100, false),
            (1, true),
            (16, true),
            (100, true),
        ] {
            let (pool, _) = self.mirrors(watchers, "live", "hash").await;
            // Held mirrors run nothing until the writes are done: the
            // writer pays only for queueing their notices.
            let waits: Vec<_> = if held {
                pool.iter()
                    .map(|p| {
                        let p = p.clone();
                        let ms = 4_000 + 250 * watchers;
                        tokio::spawn(async move { ask(&p, &format!("wait:{ms}")).await })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut per = Vec::new();
            let mut drain = 0;
            let rounds = if held { 1 } else { 3 };
            let mut stop = 0;
            for _ in 0..rounds {
                let ns: f64 = ask(&writer, &format!("write:live:{KEYS}:{KEYS}"))
                    .await
                    .parse()
                    .unwrap();
                stop = wall_ns();
                per.push(ns / KEYS as f64);
                if watchers > 0 && !held {
                    drain = Self::drained(&pool).await.saturating_sub(stop);
                }
            }
            for w in waits {
                w.await.unwrap();
            }
            if held {
                drain = Self::drained(&pool).await.saturating_sub(stop);
            }
            let (stats, _) = all(&pool, |_| "stats".into()).await;
            let notices: f64 = stats.iter().map(|s| fields(s)[1] as f64).sum::<f64>()
                / watchers.max(1) as f64
                / (rounds * KEYS) as f64;
            println!(
                "  {watchers:>3} mirrors{}: {:>6.0} ns per write; each mirror got notices for {:>3.0}% of writes; drained {} after the writer",
                if held { ", held" } else { "" },
                median(per),
                100.0 * notices,
                if held { "-".into() } else { us(drain as f64) }
            );
            for h in &pool {
                h.kill();
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        writer.kill();
        holder.kill();
    }

    async fn resume(&self) {
        println!(
            "\n== building a mirror of 100,000 keys while another instance inserts and deletes a key 1,000 times a second"
        );
        let churner = self.spawn();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = {
            let (churner, stop) = (churner.clone(), stop.clone());
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(1));
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    tick.tick().await;
                    ask(&churner, "churn:live:1").await;
                }
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        for mode in ["resume", "watch"] {
            let h = self.spawn();
            let t = Instant::now();
            let reply = ask_raw(&h, &format!("mirror:live:k:{mode}:hash")).await;
            let took = t.elapsed();
            if reply.starts_with("err:") {
                println!("  {mode:<6}: failed after {took:.0?}: {reply}");
            } else {
                let f = fields(&reply);
                println!(
                    "  {mode:<6}: {} entries in {took:.0?}, {} rescans",
                    f[0], f[2]
                );
            }
            h.kill();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        task.await.unwrap();
        churner.kill();
    }
}

fn bucket_keys(bucket: &str) -> u64 {
    if bucket == "b1k" { 1_000 } else { KEYS }
}

fn us(ns: f64) -> String {
    if ns >= 1e6 {
        format!("{:.1} ms", ns / 1e6)
    } else {
        format!("{:.0} us", ns / 1e3)
    }
}

/// User and system CPU seconds of this process.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = after.split_whitespace().collect();
    // utime and stime are fields 14 and 15; `after` starts at field 3.
    let ticks: f64 = f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap();
    ticks / 100.0
}

/// System CPU seconds of this process.
fn sys_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = after.split_whitespace().collect();
    f[12].parse::<f64>().unwrap() / 100.0
}

struct Mem {
    pss: i64,
    anon: i64,
}

impl Mem {
    fn now() -> Self {
        let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap();
        let field = |name: &str| -> i64 {
            rollup
                .lines()
                .find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(':')))
                .map_or(0, |r| {
                    r.trim().trim_end_matches(" kB").trim().parse().unwrap()
                })
        };
        Self {
            pss: field("Pss"),
            anon: field("Pss_Anon"),
        }
    }

    fn pss_mib(&self, base: &Self) -> f64 {
        (self.pss - base.pss) as f64 / 1024.0
    }

    fn anon_mib(&self, base: &Self) -> f64 {
        (self.anon - base.anon) as f64 / 1024.0
    }
}
