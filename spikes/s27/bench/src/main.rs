//! Spike S27: a reference table in the component's data segment. Loads a
//! component built by `build.sh`, starts 1, 16 and 100 instances that each
//! read every page of the table, and reports the process's memory
//! (`/proc/self/smaps_rollup`: PSS counts a physical page once however many
//! instances map it, RSS once per mapping), the time to compile, load,
//! start and read, reads per second from 1, 4 and 16 instances, slot reuse,
//! and an upgrade to a build with other data.
//!
//! s27-bench <component.wasm> [<v2 component.wasm>]
//!
//! S27_MEMFD=1 loads without the compile cache, so the image is a memfd
//! rather than the cache file (the node always uses the cache).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bulkhead_host::{Envelope, InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

const LONG: Duration = Duration::from_secs(120);
const READS: u64 = 200_000;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let memfd = std::env::var("S27_MEMFD").is_ok_and(|v| v == "1");
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 128,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let cache_dir =
        PathBuf::from(std::env::var("S27_CACHE").unwrap_or_else(|_| "target/s27-cache".into()));
    // S27_REUSE=1 keeps the cache of an earlier run, so the first load
    // reads it too, in a process that never compiled.
    if !std::env::var("S27_REUSE").is_ok_and(|v| v == "1") {
        let _ = std::fs::remove_dir_all(&cache_dir);
    }
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::set_permissions(
        &cache_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();

    let before = Mem::now();
    let component = std::fs::read(&args[0]).unwrap();
    let cache = cache_dir.join("v1.cwasm");
    let t = Instant::now();
    let first = if memfd {
        rt.load(&component).unwrap()
    } else {
        rt.load_cached(&component, &cache).unwrap()
    };
    let compile = t.elapsed();
    drop(first);
    let t = Instant::now();
    let app = if memfd {
        rt.load(&component).unwrap()
    } else {
        rt.load_cached(&component, &cache).unwrap()
    };
    let reload = t.elapsed();
    let image_bytes = component.len() as u64;
    drop(component);
    println!("{}", args[0]);
    println!(
        "  component {} MiB, {} (compile{}) {compile:.2?}, again {reload:.2?}",
        image_bytes >> 20,
        if memfd {
            "memfd image"
        } else {
            "cache-file image"
        },
        if memfd { "" } else { " and write the cache" },
    );
    let loaded = Mem::now();
    println!("  loaded:            {}", loaded.since(&before));

    // memory_max: the table plus 64 MiB for the stack and heap.
    let spec = |version: &str| InstanceSpec {
        memory_max: image_bytes + (64 << 20),
        ..InstanceSpec::new("s27", version)
    };
    let mut instances: Vec<InstanceHandle> = Vec::new();
    for target in [1usize, 16, 100] {
        let (starts, touches) = start(&rt, &app, &spec("1.0.0"), &mut instances, target).await;
        println!(
            "  {target:>3} instances:     {}; start p50 {:.2?}, read every page p50 {:.2?} (first {:.2?}); charged {} MiB each",
            Mem::now().since(&before),
            p50(starts),
            p50(touches.clone()),
            touches[0],
            instances[0].memory_bytes() >> 20,
        );
    }
    let info = text(&instances[0], "info").await;
    println!("  table: {info} (records, bytes, version)");

    // Read speed on one instance, then from 1, 4 and 16 at once.
    let get = median(&instances[0], &format!("get:{READS}:7")).await;
    let chain = median(&instances[0], &format!("chain:{READS}")).await;
    let hget = median(&instances[0], &format!("hget:{READS}:7")).await;
    let hchain = median(&instances[0], &format!("hchain:{READS}")).await;
    println!(
        "  read: {get:.0} ns, dependent {chain:.0} ns (binary search); {hget:.0} ns, dependent {hchain:.0} ns (hash index); median of 5 runs of {READS}"
    );
    for n in [1usize, 4, 16] {
        let t = Instant::now();
        let calls: Vec<_> = instances[..n]
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let h = h.clone();
                tokio::spawn(async move { text(&h, &format!("hget:{READS}:{i}")).await })
            })
            .collect();
        for c in calls {
            c.await.unwrap();
        }
        let secs = t.elapsed().as_secs_f64();
        println!(
            "  {n:>2} instances reading (hash index): {:.2}M reads/s",
            (READS * n as u64) as f64 / secs / 1e6
        );
    }

    // Slot reuse: the same 100 slots again.
    for h in instances.drain(..) {
        h.kill();
    }
    settle(&rt, 0).await;
    println!("  all stopped:       {}", Mem::now().since(&before));
    let (_, touches) = start(&rt, &app, &spec("1.0.0"), &mut instances, 100).await;
    println!(
        "  100 again:         {}; read every page p50 {:.2?}",
        Mem::now().since(&before),
        p50(touches)
    );

    if let Some(v2) = args.get(1) {
        let component = std::fs::read(v2).unwrap();
        let cache = cache_dir.join("v2.cwasm");
        let t = Instant::now();
        let first = if memfd {
            rt.load(&component).unwrap()
        } else {
            rt.load_cached(&component, &cache).unwrap()
        };
        let compile = t.elapsed();
        println!("  v2 compiled:       {}", Mem::now().since(&before));
        // S27_V2_FIRST=1 upgrades to the component as compiled, as a node
        // does on the first deploy of a release; otherwise to the one read
        // back from the cache, as after a restart.
        let next = if memfd || std::env::var("S27_V2_FIRST").is_ok_and(|v| v == "1") {
            first
        } else {
            drop(first);
            rt.load_cached(&component, &cache).unwrap()
        };
        drop(component);
        println!("  v2 loaded:         {}", Mem::now().since(&before));
        for _ in 0..3 {
            text(&instances[0], "count").await;
        }
        let mut pauses = Vec::new();
        for h in &instances {
            pauses.push(h.upgrade(&next, spec("2.0.0"), "1.0.0").await.unwrap());
        }
        let after = text(&instances[0], "info").await;
        let count = text(&instances[0], "count").await;
        let mut touches = Vec::new();
        for h in &instances {
            let t = Instant::now();
            text(h, "touch").await;
            touches.push(t.elapsed());
        }
        println!(
            "  upgrade: compile {compile:.2?}; pause first {:.2?}, p50 {:.2?}, max {:.2?}; table now {after}, count {count} (5 expected); read every page p50 {:.2?}",
            pauses[0],
            p50(pauses.clone()),
            pauses.iter().max().unwrap(),
            p50(touches),
        );
        println!("  upgraded, v1 held: {}", Mem::now().since(&before));
        drop(app);
        tokio::time::sleep(Duration::from_millis(200)).await;
        println!("  v1 dropped:        {}", Mem::now().since(&before));
        let released = rt.release_idle_memory().await;
        println!(
            "  idle slots released ({} MiB): {}",
            released >> 20,
            Mem::now().since(&before)
        );
        // Before Q15's fix an instance's task held the version it started
        // with until it exited; this shows what is left once none run.
        for h in instances.drain(..) {
            h.kill();
        }
        settle(&rt, 0).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        println!("  all stopped:       {}", Mem::now().since(&before));
        drop(next);
        println!("  v2 dropped:        {}", Mem::now().since(&before));
    }
    for h in instances.drain(..) {
        h.kill();
    }
}

/// Starts instances until there are `target`, each reading every page.
async fn start(
    rt: &Runtime,
    app: &LoadedApp,
    spec: &InstanceSpec,
    instances: &mut Vec<InstanceHandle>,
    target: usize,
) -> (Vec<Duration>, Vec<Duration>) {
    let (mut starts, mut touches) = (Vec::new(), Vec::new());
    while instances.len() < target {
        let t = Instant::now();
        let h = rt.spawn(app, spec.clone()).unwrap();
        text(&h, "count").await;
        starts.push(t.elapsed());
        let t = Instant::now();
        text(&h, "touch").await;
        touches.push(t.elapsed());
        instances.push(h);
    }
    (starts, touches)
}

async fn settle(rt: &Runtime, stores: usize) {
    for _ in 0..500 {
        if rt.store_count() <= stores {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{} stores still held", rt.store_count());
}

async fn text(h: &InstanceHandle, msg: &str) -> String {
    let reply = h
        .call(Envelope::new("text/plain", msg.to_string()), LONG)
        .await
        .unwrap();
    String::from_utf8(reply.body.to_vec()).unwrap()
}

async fn median(h: &InstanceHandle, msg: &str) -> f64 {
    let mut runs = Vec::new();
    for _ in 0..5 {
        runs.push(text(h, msg).await.parse::<f64>().unwrap() / READS as f64);
    }
    runs.sort_by(f64::total_cmp);
    runs[2]
}

fn p50(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// The process's memory: `smaps_rollup` and two system-wide counters.
struct Mem {
    rss: i64,
    pss: i64,
    anon: i64,
    file: i64,
    shmem: i64,
    available: i64,
}

impl Mem {
    fn now() -> Self {
        let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap();
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap();
        let field = |text: &str, name: &str| -> i64 {
            text.lines()
                .find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(':')))
                .map_or(0, |r| {
                    r.trim().trim_end_matches(" kB").trim().parse().unwrap()
                })
        };
        Self {
            rss: field(&rollup, "Rss"),
            pss: field(&rollup, "Pss"),
            anon: field(&rollup, "Pss_Anon"),
            file: field(&rollup, "Pss_File"),
            shmem: field(&rollup, "Pss_Shmem"),
            available: field(&meminfo, "MemAvailable"),
        }
    }

    /// MiB grown since `base`.
    fn since(&self, base: &Self) -> String {
        let m = |kb: i64| kb as f64 / 1024.0;
        format!(
            "PSS {:+.0} MiB (anon {:+.0}, file {:+.0}, shmem {:+.0}), RSS {:+.0}, MemAvailable {:+.0}",
            m(self.pss - base.pss),
            m(self.anon - base.anon),
            m(self.file - base.file),
            m(self.shmem - base.shmem),
            m(self.rss - base.rss),
            m(self.available - base.available),
        )
    }
}
