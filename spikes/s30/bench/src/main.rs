//! Spike S30: what an instance is. The same workloads against shape A, a
//! process per instance; shape B, one process hosting many instances on 4
//! worker threads (`s30-app --multi`); and shape B2, one process with a
//! thread per instance (`--multi --per-instance`); all behind the same
//! small host: an instance task
//! per instance that runs one callback at a time, frames routed by
//! instance, and calls between instances routed through the host, as the
//! node routes them. `real` measures today's native instances through
//! `bulkhead-host` (`bulkhead-runner` and `bulkhead-fixture-native`) to
//! calibrate shape A.
//!
//! s30-bench <s30-app> [phase ...]       phases: start memory calls crash real transport
//!
//! `transport` echoes 64-byte frames through an app process over its
//! pipes, over two rings in shared memory woken by eventfds, and over the
//! rings with both sides spinning.
//!
//! S30_MAX=<n> caps the instance counts of `memory` (5,000 by default);
//! S30_RUNNER and S30_FIXTURE name the real host's programs (default:
//! the main workspace's target/release).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

const SPAWN: u8 = 1;
const CALL: u8 = 2;
const REPLY: u8 = 3;
const READY: u8 = 4;
const EXIT: u8 = 5;

fn encode(kind: u8, inst: u32, id: u64, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(17 + body.len());
    f.extend_from_slice(&(13 + body.len() as u32).to_le_bytes());
    f.push(kind);
    f.extend_from_slice(&inst.to_le_bytes());
    f.extend_from_slice(&id.to_le_bytes());
    f.extend_from_slice(body);
    f
}

type Answer = Result<Vec<u8>, String>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    A,
    B,
    B2,
}

/// One app process: its writer queue and the frames it owes.
struct Proc {
    pid: u32,
    out: mpsc::UnboundedSender<Vec<u8>>,
    pending: Mutex<HashMap<(u32, u64), oneshot::Sender<Answer>>>,
    exited: tokio::sync::Notify,
    exited_at: Mutex<Option<Instant>>,
    /// Taken and reaped when the process exits.
    child: Mutex<Option<Child>>,
}

impl Proc {
    async fn request(&self, kind: u8, inst: u32, body: &[u8]) -> Answer {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            // Checked under the lock the reader drains at exit, so no
            // request is left waiting on a process that has gone.
            let mut pending = self.pending.lock().unwrap();
            if self.exited_at.lock().unwrap().is_some() {
                return Err("exited".into());
            }
            pending.insert((inst, id), tx);
        }
        if self.out.send(encode(kind, inst, id, body)).is_err() {
            return Err("exited".into());
        }
        rx.await.unwrap_or_else(|_| Err("exited".into()))
    }
}

static NEXT: AtomicU64 = AtomicU64::new(1);

/// An instance: its process and its mailbox, served by a task that sends
/// one call at a time.
#[derive(Clone)]
struct Inst {
    proc_: Arc<Proc>,
    mailbox: mpsc::UnboundedSender<(Vec<u8>, oneshot::Sender<Answer>)>,
}

impl Inst {
    async fn call(&self, body: Vec<u8>) -> Answer {
        let (tx, rx) = oneshot::channel();
        self.mailbox
            .send((body, tx))
            .map_err(|_| "dead".to_string())?;
        rx.await.unwrap_or_else(|_| Err("dead".into()))
    }
}

/// The small host: instances by number.
#[derive(Default)]
struct Host {
    insts: RwLock<HashMap<u32, Inst>>,
}

impl Host {
    fn inst(&self, n: u32) -> Option<Inst> {
        self.insts.read().unwrap().get(&n).cloned()
    }

    /// Start an app process, with its reader and writer tasks.
    fn process(self: &Arc<Self>, app: &str, shape: Shape) -> Arc<Proc> {
        let mut cmd = Command::new(app);
        match shape {
            Shape::A => {}
            Shape::B => {
                cmd.args(["--multi", "--workers", "4"]);
            }
            Shape::B2 => {
                cmd.args(["--multi", "--per-instance"]);
            }
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (out, mut queue) = mpsc::unbounded_channel::<Vec<u8>>();
        let p = Arc::new(Proc {
            pid: child.id().unwrap(),
            out,
            pending: Mutex::default(),
            exited: tokio::sync::Notify::new(),
            exited_at: Mutex::default(),
            child: Mutex::new(Some(child)),
        });
        // Writer: drain the queue, flush once per batch.
        tokio::spawn(async move {
            let mut w = BufWriter::new(stdin);
            while let Some(first) = queue.recv().await {
                if w.write_all(&first).await.is_err() {
                    return;
                }
                while let Ok(more) = queue.try_recv() {
                    let _ = w.write_all(&more).await;
                }
                if w.flush().await.is_err() {
                    return;
                }
            }
        });
        let (host, proc_) = (self.clone(), p.clone());
        tokio::spawn(async move {
            let mut r = BufReader::new(stdout);
            loop {
                let mut len = [0u8; 4];
                if r.read_exact(&mut len).await.is_err() {
                    break;
                }
                let mut rest = vec![0u8; u32::from_le_bytes(len) as usize];
                if r.read_exact(&mut rest).await.is_err() {
                    break;
                }
                let kind = rest[0];
                let inst = u32::from_le_bytes(rest[1..5].try_into().unwrap());
                let id = u64::from_le_bytes(rest[5..13].try_into().unwrap());
                let body = rest[13..].to_vec();
                match kind {
                    READY | REPLY | EXIT => {
                        let answer = if kind == EXIT {
                            Err(String::from_utf8_lossy(&body).into_owned())
                        } else {
                            Ok(body)
                        };
                        if let Some(tx) = proc_.pending.lock().unwrap().remove(&(inst, id)) {
                            let _ = tx.send(answer);
                        }
                    }
                    CALL => {
                        // An instance calls another, through the host.
                        let (host, proc_) = (host.clone(), proc_.clone());
                        tokio::spawn(async move {
                            let text = String::from_utf8(body).unwrap();
                            let (to, payload) = text.split_once('|').unwrap();
                            let answer = match host.inst(to.parse().unwrap()) {
                                Some(t) => t.call(payload.as_bytes().to_vec()).await,
                                None => Err("noproc".into()),
                            };
                            let body = answer.unwrap_or_else(|e| format!("err:{e}").into_bytes());
                            let _ = proc_.out.send(encode(REPLY, inst, id, &body));
                        });
                    }
                    _ => {}
                }
            }
            let mut pending = proc_.pending.lock().unwrap();
            *proc_.exited_at.lock().unwrap() = Some(Instant::now());
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err("exited".into()));
            }
            drop(pending);
            if let Some(mut c) = proc_.child.lock().unwrap().take() {
                tokio::spawn(async move {
                    let _ = c.wait().await;
                });
            }
            proc_.exited.notify_waiters();
        });
        p
    }

    /// Start instance `n` in `proc_` and wait for it to be ready.
    async fn start(self: &Arc<Self>, proc_: &Arc<Proc>, n: u32) {
        proc_.request(SPAWN, n, b"").await.unwrap();
        let (mailbox, mut rx) = mpsc::unbounded_channel::<(Vec<u8>, oneshot::Sender<Answer>)>();
        let p = proc_.clone();
        tokio::spawn(async move {
            while let Some((body, reply)) = rx.recv().await {
                let _ = reply.send(p.request(CALL, n, &body).await);
            }
        });
        self.insts.write().unwrap().insert(
            n,
            Inst {
                proc_: proc_.clone(),
                mailbox,
            },
        );
    }

    fn stop_all(&self) {
        self.insts.write().unwrap().clear();
    }
}

/// Shape A: `n` processes of one instance each, numbered from `from`.
async fn shape_a(host: &Arc<Host>, app: &str, from: u32, n: u32) -> Vec<Arc<Proc>> {
    let mut procs = Vec::new();
    for i in from..from + n {
        let p = host.process(app, Shape::A);
        host.start(&p, i).await;
        procs.push(p);
    }
    procs
}

/// Shape B or B2: one process of `n` instances.
async fn shape_b(host: &Arc<Host>, app: &str, n: u32, shape: Shape) -> Arc<Proc> {
    let p = host.process(app, shape);
    let starts: Vec<_> = (0..n)
        .map(|i| {
            let (host, p) = (host.clone(), p.clone());
            tokio::spawn(async move { host.start(&p, i).await })
        })
        .collect();
    for s in starts {
        s.await.unwrap();
    }
    p
}

/// End these app processes. Each process's reader task holds its `Proc`
/// until the process exits, so dropping the handles does not.
fn kill(pids: &[u32]) {
    for p in pids {
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(p.to_string())
            .status();
    }
}

fn percentiles(mut v: Vec<Duration>) -> String {
    v.sort();
    let at = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    format!("p50 {:.1?}, p99 {:.1?}", at(0.5), at(0.99))
}

/// PSS in KiB of these processes.
fn pss_kib(pids: &[u32]) -> u64 {
    pids.iter()
        .map(|p| {
            std::fs::read_to_string(format!("/proc/{p}/smaps_rollup"))
                .ok()
                .and_then(|s| {
                    s.lines().find_map(|l| {
                        l.strip_prefix("Pss:")
                            .map(|r| r.trim().trim_end_matches(" kB").trim().parse().unwrap())
                    })
                })
                .unwrap_or(0)
        })
        .sum()
}

/// `MemAvailable` in KiB: what the kernel's own memory per process and
/// per thread (page tables, kernel stacks) costs, which PSS leaves out.
fn available_kib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .map(|r| r.trim().trim_end_matches(" kB").trim().parse().unwrap())
        .unwrap()
}

fn self_pss_kib() -> u64 {
    pss_kib(&[std::process::id()])
}

/// CPU seconds of these processes (user and system).
fn cpu(pids: &[u32]) -> f64 {
    pids.iter()
        .map(|p| {
            let stat = std::fs::read_to_string(format!("/proc/{p}/stat")).unwrap_or_default();
            let Some(i) = stat.rfind(')') else { return 0.0 };
            let f: Vec<&str> = stat[i + 2..].split_whitespace().collect();
            (f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap()) / 100.0
        })
        .sum()
}

/// Children of this process (the native runner execs the app, so each
/// instance is a direct child).
fn children() -> Vec<u32> {
    let me = std::process::id();
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|p| {
            std::fs::read_to_string(format!("/proc/{p}/stat"))
                .ok()
                .and_then(|s| {
                    let i = s.rfind(')')?;
                    s[i + 2..].split_whitespace().nth(1)?.parse::<u32>().ok()
                })
                == Some(me)
        })
        .collect()
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    // Two pipes per app process: shape A at 5,000 needs more descriptors
    // than an ssh session's soft limit of 1,024.
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit and setrlimit with a valid struct.
    unsafe {
        libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim);
        lim.rlim_cur = lim.rlim_max;
        libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let app = args[0].clone();
    let phases: Vec<&str> = if args.len() > 1 {
        args[1..].iter().map(String::as_str).collect()
    } else {
        vec!["start", "memory", "calls", "crash", "real", "transport"]
    };
    println!(
        "4 tokio workers; shape B runs 4 worker threads; 4 KiB of state per instance; GLIBC_TUNABLES={}",
        std::env::var("GLIBC_TUNABLES").unwrap_or_default()
    );
    for phase in phases {
        match phase {
            "start" => start(&app).await,
            "memory" => memory(&app).await,
            "calls" => calls(&app).await,
            "crash" => crash(&app).await,
            "real" => real::run().await,
            "transport" => transport::run(&app).await,
            other => panic!("unknown phase {other}"),
        }
    }
}

async fn start(app: &str) {
    println!("\n== start: one instance at a time, until it answers ready");
    let host = Arc::new(Host::default());
    let mut a = Vec::new();
    for i in 0..200 {
        let t = Instant::now();
        let p = host.process(app, Shape::A);
        host.start(&p, i).await;
        a.push(t.elapsed());
        kill(&[p.pid]);
    }
    host.stop_all();
    println!("  A, a process each:        {}", percentiles(a));
    for shape in [Shape::B, Shape::B2] {
        let host = Arc::new(Host::default());
        let t = Instant::now();
        let p = host.process(app, shape);
        host.start(&p, 0).await;
        let first = t.elapsed();
        let mut b = Vec::new();
        for i in 1..=2000 {
            let t = Instant::now();
            host.start(&p, i).await;
            b.push(t.elapsed());
        }
        println!(
            "  {shape:?}, in a running process: {} (the process and its first instance: {first:.1?})",
            percentiles(b)
        );
        let t = Instant::now();
        let host2 = Arc::new(Host::default());
        let p2 = shape_b(&host2, app, 1000, shape).await;
        println!("  {shape:?}, 1,000 at once: {:.1?} in all", t.elapsed());
        kill(&[p.pid, p2.pid]);
    }
}

async fn memory(app: &str) {
    let max: u32 = std::env::var("S30_MAX").map_or(5000, |v| v.parse().unwrap());
    println!("\n== memory: PSS of the app processes, and of the host, per instance");
    for n in [1u32, 100, 1000, 5000].into_iter().filter(|n| *n <= max) {
        for shape in [Shape::A, Shape::B, Shape::B2] {
            let host = Arc::new(Host::default());
            let base = self_pss_kib();
            let avail = available_kib();
            let t = Instant::now();
            let pids: Vec<u32> = if shape != Shape::A {
                vec![shape_b(&host, app, n, shape).await.pid]
            } else {
                let procs = shape_a(&host, app, 0, n).await;
                let pids = procs.iter().map(|p| p.pid).collect();
                std::mem::forget(procs); // kept alive by the host's instances
                pids
            };
            let took = t.elapsed();
            // Every instance answers once, so each has run a callback.
            for i in 0..n {
                host.inst(i)
                    .unwrap()
                    .call(b"echo:x".to_vec())
                    .await
                    .unwrap();
            }
            let apps = pss_kib(&pids);
            let own = self_pss_kib().saturating_sub(base);
            let gone = avail.saturating_sub(available_kib());
            println!(
                "  {:<2} {n:>5} instances: apps {:>8.1} KiB each, host {:>6.1} KiB each, MemAvailable {:>7.1} KiB less each; started in {took:.0?}",
                format!("{shape:?}"),
                apps as f64 / n as f64,
                own as f64 / n as f64,
                gone as f64 / n as f64,
            );
            kill(&pids);
            host.stop_all();
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}

async fn calls(app: &str) {
    println!("\n== calls, 100 instances");
    for shape in [Shape::A, Shape::B, Shape::B2] {
        let host = Arc::new(Host::default());
        let pids: Vec<u32> = if shape != Shape::A {
            vec![shape_b(&host, app, 100, shape).await.pid]
        } else {
            let procs = shape_a(&host, app, 0, 100).await;
            let pids = procs.iter().map(|p| p.pid).collect();
            std::mem::forget(procs);
            pids
        };
        let shape = format!("{shape:?}");
        let mut rng = Lcg(7);
        let mut one = Vec::new();
        for _ in 0..20_000 {
            let i = host.inst(rng.next(100) as u32).unwrap();
            let t = Instant::now();
            i.call(b"echo:x".to_vec()).await.unwrap();
            one.push(t.elapsed());
        }
        println!(
            "  {shape} host to instance, one at a time: {}",
            percentiles(one)
        );
        let mut relay = Vec::new();
        // Shape B: an instance waiting on a call holds its worker, and a
        // call to an instance on the same worker never returns.
        let relays = if shape == "B" { 0 } else { 10_000 };
        for _ in 0..relays {
            // Never itself: one callback at a time, so a call to itself
            // would wait for ever (the node refuses one).
            let x = rng.next(100) as u32;
            let y = (x + 1 + rng.next(99) as u32) % 100;
            let i = host.inst(x).unwrap();
            let t = Instant::now();
            let r = i.call(format!("relay:{y}:x").into_bytes()).await.unwrap();
            assert_eq!(r, b"x");
            relay.push(t.elapsed());
        }
        if relay.is_empty() {
            println!(
                "  {shape} instance to instance: not run (a waiting instance holds its worker)"
            );
        } else {
            println!(
                "  {shape} instance to instance, through the host: {}",
                percentiles(relay)
            );
        }
        for in_flight in [16usize, 64] {
            let cpu0 = cpu(&pids) + cpu(&[std::process::id()]);
            let t = Instant::now();
            let tasks: Vec<_> = (0..in_flight)
                .map(|k| {
                    let host = host.clone();
                    tokio::spawn(async move {
                        let mut rng = Lcg(k as u64);
                        let mut n = 0u64;
                        let until = Instant::now() + Duration::from_secs(3);
                        while Instant::now() < until {
                            let i = host.inst(rng.next(100) as u32).unwrap();
                            i.call(b"echo:x".to_vec()).await.unwrap();
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
            let cores = (cpu(&pids) + cpu(&[std::process::id()]) - cpu0) / wall;
            println!(
                "  {shape} {in_flight} calls in flight: {:.0} calls/s, {cores:.2} cores, {:.1} µs of CPU per call",
                total as f64 / wall,
                cores * 1e6 / (total as f64 / wall)
            );
        }
        kill(&pids);
        host.stop_all();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn crash(app: &str) {
    println!("\n== crashes");
    // A: one process of 100 aborts.
    let host = Arc::new(Host::default());
    let procs = shape_a(&host, app, 0, 100).await;
    let victim = host.inst(0).unwrap();
    let t = Instant::now();
    let _ = victim.call(b"abort".to_vec()).await;
    let seen = t.elapsed();
    let t = Instant::now();
    let p = host.process(app, Shape::A);
    host.start(&p, 0).await;
    let back = t.elapsed();
    let mut alive = 0;
    for i in 0..100 {
        if host.inst(i).unwrap().call(b"echo:x".to_vec()).await.is_ok() {
            alive += 1;
        }
    }
    println!("  A abort: seen in {seen:.2?}, replaced in {back:.2?}; {alive} of 100 answer after");
    kill(
        &procs
            .iter()
            .map(|p| p.pid)
            .chain([p.pid])
            .collect::<Vec<_>>(),
    );
    host.stop_all();
    for (shape, n) in [(Shape::B, 100u32), (Shape::B, 1000), (Shape::B2, 1000)] {
        let host = Arc::new(Host::default());
        let p = shape_b(&host, app, n, shape).await;
        // A panic in one instance: shape B catches it and ends that one.
        let t = Instant::now();
        let r = host.inst(0).unwrap().call(b"panic".to_vec()).await;
        let seen = t.elapsed();
        let t = Instant::now();
        host.start(&p, 0).await;
        let back = t.elapsed();
        println!(
            "  {shape:?} {n} instances, panic in one: {r:?} in {seen:.2?}, replaced in {back:.2?}"
        );
        // An abort ends the process and every instance in it.
        let t = Instant::now();
        let _ = host.inst(1).unwrap().call(b"abort".to_vec()).await;
        let seen = t.elapsed();
        let mut lost = 0;
        for i in 0..n {
            if host
                .inst(i)
                .unwrap()
                .call(b"echo:x".to_vec())
                .await
                .is_err()
            {
                lost += 1;
            }
        }
        host.stop_all();
        let t = Instant::now();
        let p2 = shape_b(&host, app, n, shape).await;
        let back = t.elapsed();
        kill(&[p2.pid]);
        println!(
            "  {shape:?} {n} instances, abort: seen in {seen:.2?}; {lost} of {n} lost; all restarted in {back:.2?}"
        );
        drop(p);
        host.stop_all();
    }
}

mod real {
    //! Today's native instances through `bulkhead-host`.

    use super::*;
    use bulkhead_bundle::manifest::NativeProgram;
    use bulkhead_host::{
        Envelope, InstanceHandle, InstanceSpec, NativeCode, NativeConfig, Runtime, RuntimeConfig,
    };

    fn path(var: &str, default: &str) -> PathBuf {
        std::env::var_os(var).map_or_else(
            || {
                PathBuf::from(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../target/release"
                ))
                .join(default)
            },
            PathBuf::from,
        )
    }

    async fn ask(h: &InstanceHandle, body: &str) -> Result<String, String> {
        h.call(Envelope::new("text/plain", body), Duration::from_secs(10))
            .await
            .map(|e| String::from_utf8(e.body).unwrap())
            .map_err(|e| format!("{e:?}"))
    }

    pub async fn run() {
        println!("\n== real: today's native instances (bulkhead-runner, bulkhead-fixture-native)");
        let mut native = NativeConfig::default();
        native.runner = Some(path("S30_RUNNER", "bulkhead-runner"));
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 1100,
            native,
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load_native(NativeCode {
            path: path("S30_FIXTURE", "bulkhead-fixture-native"),
            kind: NativeProgram::Executable,
        });
        let spec = || InstanceSpec {
            memory_max: 64 << 20,
            callback_deadline: Duration::from_secs(5),
            init_timeout: Duration::from_secs(10),
            ..InstanceSpec::new("native", "1.0.0")
        };
        let mut starts = Vec::new();
        let mut hs = Vec::new();
        for _ in 0..100 {
            let t = Instant::now();
            let h = rt.spawn(&app, spec()).unwrap();
            ask(&h, "echo x").await.unwrap();
            starts.push(t.elapsed());
            hs.push(h);
        }
        println!("  start, to the first answer: {}", percentiles(starts));
        let pids = children();
        println!(
            "  100 instances: {} processes, {:.1} KiB PSS each",
            pids.len(),
            pss_kib(&pids) as f64 / hs.len() as f64
        );
        let mut rng = Lcg(7);
        let mut one = Vec::new();
        for _ in 0..20_000 {
            let h = &hs[rng.next(100) as usize];
            let t = Instant::now();
            ask(h, "echo x").await.unwrap();
            one.push(t.elapsed());
        }
        println!("  host to instance, one at a time: {}", percentiles(one));
        let mut relay = Vec::new();
        for _ in 0..10_000 {
            let i = rng.next(100) as usize;
            let (x, y) = (&hs[i], hs[(i + 1 + rng.next(99) as usize) % 100].pid());
            let t = Instant::now();
            let r = ask(
                x,
                &format!("call {}.{}.{} echo x", y.node, y.id, y.incarnation),
            )
            .await
            .unwrap();
            assert_eq!(r, "x");
            relay.push(t.elapsed());
        }
        println!("  instance to instance: {}", percentiles(relay));
        for in_flight in [16usize, 64] {
            let cpu0 = cpu(&pids) + cpu(&[std::process::id()]);
            let t = Instant::now();
            let tasks: Vec<_> = (0..in_flight)
                .map(|k| {
                    let hs = hs.clone();
                    tokio::spawn(async move {
                        let mut rng = Lcg(k as u64);
                        let mut n = 0u64;
                        let until = Instant::now() + Duration::from_secs(3);
                        while Instant::now() < until {
                            ask(&hs[rng.next(100) as usize], "echo x").await.unwrap();
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
            let cores = (cpu(&pids) + cpu(&[std::process::id()]) - cpu0) / wall;
            println!(
                "  {in_flight} calls in flight: {:.0} calls/s, {cores:.2} cores, {:.1} µs of CPU per call",
                total as f64 / wall,
                cores * 1e6 / (total as f64 / wall)
            );
        }
        let t = Instant::now();
        let victim = hs[0].clone();
        let _ = ask(&victim, "abort").await;
        let reason = victim.wait().await;
        println!("  abort: exit seen in {:.2?} ({reason:?})", t.elapsed());
        for h in &hs {
            h.kill();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let max: usize =
            std::env::var("S30_MAX").map_or(1000, |v| v.parse::<usize>().unwrap().min(1000));
        let mut hs = Vec::new();
        let avail = available_kib();
        let t = Instant::now();
        for _ in 0..max {
            hs.push(rt.spawn(&app, spec()).unwrap());
        }
        for h in &hs {
            ask(h, "echo x").await.unwrap();
        }
        let pids = children();
        println!(
            "  {max} instances: started in {:.1?}, {} processes, {:.1} KiB PSS each, MemAvailable {:.1} KiB less each",
            t.elapsed(),
            pids.len(),
            pss_kib(&pids) as f64 / max as f64,
            avail.saturating_sub(available_kib()) as f64 / max as f64
        );
        for h in &hs {
            h.kill();
        }
    }
}

#[path = "../../app/src/ring.rs"]
mod ring;

mod transport {
    //! The frame transport alone: an app process echoing frames.

    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::atomic::AtomicBool;

    use tokio::io::unix::AsyncFd;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::ring::{RING, Ring, drain, wake};

    const SAMPLES: usize = 20_000;
    const WINDOW: usize = 64;
    const SECONDS: u64 = 2;

    fn frame() -> Vec<u8> {
        let mut f = (60u32).to_le_bytes().to_vec();
        f.extend_from_slice(&[7u8; 60]);
        f
    }

    fn report(name: &str, rtt: Vec<Duration>, per_s: f64, cores: f64) {
        println!(
            "  {name:<22} round trip {}; {WINDOW} in flight: {:.0}k frames/s, {:.1} µs of CPU per frame",
            percentiles(rtt),
            per_s / 1e3,
            cores * 1e6 / per_s
        );
    }

    async fn pipe(app: &str) {
        let mut child = Command::new(app)
            .args(["--echo", "pipe"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let mut w = BufWriter::new(child.stdin.take().unwrap());
        let mut r = BufReader::new(child.stdout.take().unwrap());
        let f = frame();
        let mut back = vec![0u8; f.len()];
        let mut rtt = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let t = Instant::now();
            w.write_all(&f).await.unwrap();
            w.flush().await.unwrap();
            r.read_exact(&mut back).await.unwrap();
            rtt.push(t.elapsed());
        }
        let window = Arc::new(Semaphore::new(WINDOW));
        let stop = Arc::new(AtomicBool::new(false));
        let cpu0 = cpu(&[pid, std::process::id()]);
        let t = Instant::now();
        let writer = {
            let (window, stop, f) = (window.clone(), stop.clone(), f.clone());
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    match window.try_acquire() {
                        Ok(p) => p.forget(),
                        Err(_) => {
                            w.flush().await.unwrap();
                            window.acquire().await.unwrap().forget();
                        }
                    }
                    w.write_all(&f).await.unwrap();
                }
                w
            })
        };
        let mut n = 0u64;
        while t.elapsed() < Duration::from_secs(SECONDS) {
            r.read_exact(&mut back).await.unwrap();
            window.add_permits(1);
            n += 1;
        }
        let wall = t.elapsed().as_secs_f64();
        let cores = (cpu(&[pid, std::process::id()]) - cpu0) / wall;
        stop.store(true, Ordering::Relaxed);
        window.add_permits(WINDOW);
        drop(writer);
        report("pipe", rtt, n as f64 / wall, cores);
        let _ = child.kill().await;
    }

    /// Two rings in a memfd and two eventfds, inherited by the app.
    fn shared() -> (*mut u8, i32, i32, i32) {
        // SAFETY: plain syscalls; the descriptors are kept for the run.
        unsafe {
            let memfd = libc::memfd_create(c"s30-rings".as_ptr(), 0);
            assert!(memfd >= 0);
            assert_eq!(libc::ftruncate(memfd, (2 * RING) as i64), 0);
            let base = libc::mmap(
                std::ptr::null_mut(),
                2 * RING,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                memfd,
                0,
            ) as *mut u8;
            let h2c = libc::eventfd(0, 0);
            let c2h = libc::eventfd(0, libc::EFD_NONBLOCK);
            (base, memfd, h2c, c2h)
        }
    }

    struct Fd(i32);
    impl AsRawFd for Fd {
        fn as_raw_fd(&self) -> i32 {
            self.0
        }
    }

    async fn ring_wake(app: &str) {
        let (base, memfd, h2c_fd, c2h_fd) = shared();
        let mut child = Command::new(app)
            .args(["--echo", &format!("ring:{memfd}:{h2c_fd}:{c2h_fd}")])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        // SAFETY: the mapping outlives both rings (never unmapped).
        let (h2c, c2h) = unsafe {
            (
                Arc::new(Ring::new(base)),
                Arc::new(Ring::new(base.add(RING))),
            )
        };
        let ready = Arc::new(AsyncFd::new(Fd(c2h_fd)).unwrap());
        let recv = |c2h: Arc<Ring>, ready: Arc<AsyncFd<Fd>>| async move {
            loop {
                if let Some(f) = c2h.pop() {
                    return f;
                }
                if c2h.sleep() {
                    let mut guard = ready.readable().await.unwrap();
                    if !drain(c2h_fd) {
                        guard.clear_ready();
                    }
                }
            }
        };
        let f = frame();
        let mut rtt = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let t = Instant::now();
            if h2c.push(&f[4..]) {
                wake(h2c_fd);
            }
            recv(c2h.clone(), ready.clone()).await;
            rtt.push(t.elapsed());
        }
        let window = Arc::new(Semaphore::new(WINDOW));
        let stop = Arc::new(AtomicBool::new(false));
        let cpu0 = cpu(&[pid, std::process::id()]);
        let t = Instant::now();
        {
            let (window, stop, h2c) = (window.clone(), stop.clone(), h2c.clone());
            let body = f[4..].to_vec();
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    window.acquire().await.unwrap().forget();
                    if h2c.push(&body) {
                        wake(h2c_fd);
                    }
                }
            });
        }
        let mut n = 0u64;
        while t.elapsed() < Duration::from_secs(SECONDS) {
            recv(c2h.clone(), ready.clone()).await;
            window.add_permits(1);
            n += 1;
        }
        let wall = t.elapsed().as_secs_f64();
        let cores = (cpu(&[pid, std::process::id()]) - cpu0) / wall;
        stop.store(true, Ordering::Relaxed);
        window.add_permits(WINDOW);
        report("ring, eventfd wakes", rtt, n as f64 / wall, cores);
        let _ = child.kill().await;
        drop(unsafe { OwnedFd::from_raw_fd(memfd) });
    }

    /// Both sides spin: each holds a core while it waits.
    async fn ring_spin(app: &str) {
        let (base, memfd, h2c_fd, c2h_fd) = shared();
        let mut child = Command::new(app)
            .args(["--echo", &format!("ring:{memfd}:{h2c_fd}:{c2h_fd}:spin")])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let base = base as usize;
        let (rtt, per_s, cores) = tokio::task::spawn_blocking(move || {
            let base = base as *mut u8;
            // SAFETY: as in `ring_wake`.
            let (h2c, c2h) = unsafe { (Ring::new(base), Ring::new(base.add(RING))) };
            let f = frame();
            let mut rtt = Vec::with_capacity(SAMPLES);
            for _ in 0..SAMPLES {
                let t = Instant::now();
                h2c.push(&f[4..]);
                while c2h.pop().is_none() {
                    std::hint::spin_loop();
                }
                rtt.push(t.elapsed());
            }
            let cpu0 = cpu(&[pid, std::process::id()]);
            let t = Instant::now();
            let (mut sent, mut n) = (0u64, 0u64);
            while t.elapsed() < Duration::from_secs(SECONDS) {
                while sent - n < WINDOW as u64 {
                    h2c.push(&f[4..]);
                    sent += 1;
                }
                if c2h.pop().is_some() {
                    n += 1;
                }
            }
            let wall = t.elapsed().as_secs_f64();
            (
                rtt,
                n as f64 / wall,
                (cpu(&[pid, std::process::id()]) - cpu0) / wall,
            )
        })
        .await
        .unwrap();
        report("ring, both spinning", rtt, per_s, cores);
        let _ = child.kill().await;
        drop(unsafe { OwnedFd::from_raw_fd(memfd) });
    }

    pub async fn run(app: &str) {
        println!("\n== transport: 64-byte frames echoed by an app process");
        pipe(app).await;
        ring_wake(app).await;
        ring_spin(app).await;
    }
}
