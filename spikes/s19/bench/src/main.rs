//! The host side of S19: spawn `s19-runner` as a port, the way a node
//! would own a native-process plugin, and measure it.
//!
//! ```text
//! s19-bench all|start|latency|throughput|failures|sandbox|cgroup
//! ```
//!
//! Expects `target/release/s19-runner` and `target/release/libs19_plugin.so`
//! (run from `spikes/`).

use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

const CALL: u8 = 1;
const REPLY: u8 = 2;
const ERROR: u8 = 3;
const PING: u8 = 4;
const PONG: u8 = 5;
const READY: u8 = 6;

const RUNNER: &str = "target/release/s19-runner";
const PLUGIN: &str = "target/release/libs19_plugin.so";

/// How a call ended.
#[derive(Debug)]
enum Answer {
    Reply(#[allow(dead_code)] Vec<u8>),
    Error(String),
    /// The runner's stdout closed: it exited or was killed.
    Closed,
    /// No answer within the deadline.
    Timeout,
}

#[derive(Default)]
struct Options {
    workers: usize,
    sandbox: bool,
    allow_connect: Vec<u16>,
    /// `RLIMIT_AS` in bytes, set between fork and exec.
    address_space: Option<u64>,
    /// A cgroup directory to put the runner in, between fork and exec.
    cgroup: Option<PathBuf>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Answer>>>>;

/// One running plugin process.
struct Port {
    child: Child,
    frames: mpsc::UnboundedSender<Vec<u8>>,
    pending: Pending,
    next: u64,
    /// When the runner's stdout closed, once it has.
    closed: Arc<Mutex<Option<Instant>>>,
}

fn frame(kind: u8, id: u64, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(4 + 9 + body.len());
    let len = u32::try_from(9 + body.len()).unwrap_or(u32::MAX);
    f.extend_from_slice(&len.to_be_bytes());
    f.push(kind);
    f.extend_from_slice(&id.to_be_bytes());
    f.extend_from_slice(body);
    f
}

impl Port {
    /// Spawn the runner and wait for its ready frame.
    async fn spawn(o: &Options) -> Port {
        let mut cmd = Command::new(RUNNER);
        cmd.arg(PLUGIN)
            .arg("--workers")
            .arg(o.workers.max(1).to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if o.sandbox {
            cmd.arg("--sandbox");
        }
        for p in &o.allow_connect {
            cmd.arg("--allow-connect").arg(p.to_string());
        }
        let limit = o.address_space;
        let procs = o
            .cgroup
            .as_ref()
            .map(|c| CString::new(c.join("cgroup.procs").into_os_string().into_encoded_bytes()))
            .transpose()
            .ok()
            .flatten();
        // SAFETY: between fork and exec the closure only makes system
        // calls through rustix, which neither allocates nor takes locks.
        unsafe {
            cmd.pre_exec(move || {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::KILL,
                ))?;
                // No core dump: writing one delays the exit (and the
                // host's notice of it) by about a second.
                rustix::process::setrlimit(
                    rustix::process::Resource::Core,
                    rustix::process::Rlimit {
                        current: Some(0),
                        maximum: Some(0),
                    },
                )?;
                if let Some(bytes) = limit {
                    rustix::process::setrlimit(
                        rustix::process::Resource::As,
                        rustix::process::Rlimit {
                            current: Some(bytes),
                            maximum: Some(bytes),
                        },
                    )?;
                }
                if let Some(procs) = &procs {
                    let fd = rustix::fs::open(
                        procs.as_c_str(),
                        rustix::fs::OFlags::WRONLY,
                        rustix::fs::Mode::empty(),
                    )?;
                    rustix::io::write(&fd, b"0")?;
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn runner");
        let mut stdin = child.stdin.take().expect("stdin");
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let (frames, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(f) = rx.recv().await {
                if stdin.write_all(&f).await.is_err() {
                    return;
                }
            }
        });
        let pending: Pending = Arc::default();
        let closed = Arc::new(Mutex::new(None));
        let (ready_tx, ready_rx) = oneshot::channel();
        {
            let pending = pending.clone();
            let closed = closed.clone();
            tokio::spawn(async move {
                let mut ready = Some(ready_tx);
                loop {
                    let mut len = [0u8; 4];
                    if stdout.read_exact(&mut len).await.is_err() {
                        break;
                    }
                    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
                    if stdout.read_exact(&mut buf).await.is_err() {
                        break;
                    }
                    let id = u64::from_be_bytes(buf[1..9].try_into().unwrap_or_default());
                    let body = buf.split_off(9);
                    let answer = match buf[0] {
                        READY => {
                            if let Some(r) = ready.take() {
                                let _ = r.send(());
                            }
                            continue;
                        }
                        REPLY | PONG => Answer::Reply(body),
                        ERROR => Answer::Error(String::from_utf8_lossy(&body).into_owned()),
                        _ => continue,
                    };
                    let waiter = pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                    if let Some(w) = waiter {
                        let _ = w.send(answer);
                    }
                }
                *closed.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
                for (_, w) in pending.lock().unwrap_or_else(PoisonError::into_inner).drain() {
                    let _ = w.send(Answer::Closed);
                }
            });
        }
        let _ = ready_rx.await;
        Port {
            child,
            frames,
            pending,
            next: 1,
            closed,
        }
    }

    fn send(&mut self, kind: u8, body: &[u8]) -> oneshot::Receiver<Answer> {
        let id = self.next;
        self.next += 1;
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        let _ = self.frames.send(frame(kind, id, body));
        rx
    }

    fn call_frame(tag: &str, payload: &str) -> Vec<u8> {
        let mut body = Vec::with_capacity(2 + tag.len() + payload.len());
        body.extend_from_slice(&u16::try_from(tag.len()).unwrap_or(0).to_be_bytes());
        body.extend_from_slice(tag.as_bytes());
        body.extend_from_slice(payload.as_bytes());
        body
    }

    fn start_call(&mut self, tag: &str, payload: &str) -> oneshot::Receiver<Answer> {
        let body = Self::call_frame(tag, payload);
        self.send(CALL, &body)
    }

    async fn call(&mut self, tag: &str, payload: &str, deadline: Duration) -> Answer {
        let rx = self.start_call(tag, payload);
        match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(a)) => a,
            Ok(Err(_)) => Answer::Closed,
            Err(_) => Answer::Timeout,
        }
    }

    async fn ping(&mut self, deadline: Duration) -> Answer {
        let rx = self.send(PING, &[]);
        match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(a)) => a,
            Ok(Err(_)) => Answer::Closed,
            Err(_) => Answer::Timeout,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id().unwrap_or(0)
    }

    /// Close stdin and wait for the runner to exit.
    async fn stop(mut self) -> (ExitStatus, Duration) {
        let t = Instant::now();
        drop(self.frames);
        let status = self.child.wait().await.expect("wait");
        (status, t.elapsed())
    }

    fn closed_at(&self) -> Option<Instant> {
        *self.closed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn status_text(s: ExitStatus) -> String {
    match (s.code(), s.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (_, Some(sig)) => format!("signal {sig}"),
        _ => "unknown".into(),
    }
}

/// `VmRSS` and `Threads` from `/proc/<pid>/status`.
fn rss(pid: u32) -> String {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .map(|l| l[name.len()..].trim().to_string())
            .unwrap_or_default()
    };
    format!("RSS {}, {} threads", field("VmRSS:"), field("Threads:"))
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

fn summary(mut d: Vec<Duration>) -> String {
    d.sort();
    format!(
        "p50 {:.1} µs, p99 {:.1} µs, max {:.1} µs",
        percentile(&d, 0.5).as_secs_f64() * 1e6,
        percentile(&d, 0.99).as_secs_f64() * 1e6,
        percentile(&d, 1.0).as_secs_f64() * 1e6,
    )
}

fn echo_payload(bytes: usize) -> String {
    format!(r#"{{"data":"{}"}}"#, "x".repeat(bytes))
}

async fn start() {
    println!("## start: spawn to ready (plugin loaded), first call, stop");
    let mut ready = Vec::new();
    let mut first = Vec::new();
    let mut stop = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let mut port = Port::spawn(&Options::default()).await;
        ready.push(t.elapsed());
        let t = Instant::now();
        let a = port.call("echo", &echo_payload(8), Duration::from_secs(5)).await;
        assert!(matches!(a, Answer::Reply(_)), "{a:?}");
        first.push(t.elapsed());
        let (status, took) = port.stop().await;
        assert!(status.success(), "{status:?}");
        stop.push(took);
    }
    println!("spawn to ready: {}", summary(ready));
    println!("first call:     {}", summary(first));
    println!("stdin closed to exit: {}", summary(stop));
    let port = Port::spawn(&Options::default()).await;
    println!("idle runner: {}", rss(port.pid()));
    port.stop().await;
}

async fn latency() {
    println!("## latency: sequential echo calls, one worker");
    let mut port = Port::spawn(&Options::default()).await;
    for bytes in [64, 4096, 65536] {
        let payload = echo_payload(bytes);
        let n = if bytes > 4096 { 5_000 } else { 20_000 };
        for _ in 0..1000 {
            port.call("echo", &payload, Duration::from_secs(5)).await;
        }
        let mut d = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            let a = port.call("echo", &payload, Duration::from_secs(5)).await;
            d.push(t.elapsed());
            assert!(matches!(a, Answer::Reply(_)));
        }
        println!("{bytes:>6} B: {}", summary(d));
    }
    let mut d = Vec::new();
    for _ in 0..20_000 {
        let t = Instant::now();
        port.ping(Duration::from_secs(5)).await;
        d.push(t.elapsed());
    }
    println!("  ping: {} (the pipe and frame code alone)", summary(d));
    println!("after: {}", rss(port.pid()));
    port.stop().await;
}

async fn throughput() {
    println!("## throughput: 64-byte echo calls kept in flight for 2 s");
    for workers in [1, 4] {
        for in_flight in [1, 4, 16, 64] {
            let mut port = Port::spawn(&Options {
                workers,
                ..Options::default()
            })
            .await;
            let payload = echo_payload(64);
            let mut queue = std::collections::VecDeque::new();
            let end = Instant::now() + Duration::from_secs(2);
            let mut done = 0u64;
            while Instant::now() < end {
                while queue.len() < in_flight {
                    queue.push_back(port.start_call("echo", &payload));
                }
                if let Some(rx) = queue.pop_front() {
                    let _ = rx.await;
                    done += 1;
                }
            }
            println!(
                "workers {workers}, in flight {in_flight:>2}: {:>7.0} calls/s",
                done as f64 / 2.0
            );
            port.stop().await;
        }
    }
}

async fn failures() {
    println!("## failures");
    // A plugin that aborts: the pending call sees the pipe close.
    let mut port = Port::spawn(&Options::default()).await;
    let t = Instant::now();
    let a = port.call("abort", "{}", Duration::from_secs(5)).await;
    let seen = t.elapsed();
    let status = port.child.wait().await.expect("wait");
    println!(
        "abort: call ended {a:?} after {:.0} µs; {}",
        seen.as_secs_f64() * 1e6,
        status_text(status)
    );

    // Killed from outside.
    let mut port = Port::spawn(&Options::default()).await;
    let pid = port.pid();
    let t = Instant::now();
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid as i32).expect("pid"),
        rustix::process::Signal::KILL,
    )
    .expect("kill");
    let status = port.child.wait().await.expect("wait");
    let reaped = t.elapsed();
    while port.closed_at().is_none() {
        tokio::task::yield_now().await;
    }
    println!(
        "SIGKILL: reaped after {:.0} µs, stdout closed after {:.0} µs; {}",
        reaped.as_secs_f64() * 1e6,
        (port.closed_at().expect("closed") - t).as_secs_f64() * 1e6,
        status_text(status)
    );

    // A call that never returns: the call's deadline catches it, a ping
    // does not (the reading thread still answers), and the host kills.
    let mut port = Port::spawn(&Options {
        workers: 2,
        ..Options::default()
    })
    .await;
    let a = port.call("spin", "{}", Duration::from_millis(100)).await;
    let pong = port.ping(Duration::from_millis(100)).await;
    let other = port.call("echo", &echo_payload(8), Duration::from_millis(100)).await;
    let t = Instant::now();
    port.child.start_kill().expect("kill");
    let status = port.child.wait().await.expect("wait");
    println!(
        "spin: call {a:?} at its 100 ms deadline; ping meanwhile {}; a second call on the other worker {}; killed and reaped in {:.0} µs ({})",
        if matches!(pong, Answer::Reply(_)) { "answered" } else { "not answered" },
        if matches!(other, Answer::Reply(_)) { "answered" } else { "not answered" },
        t.elapsed().as_secs_f64() * 1e6,
        status_text(status)
    );

    // A runner stopped (SIGSTOP, or wedged in the kernel): only a ping
    // deadline notices.
    let mut port = Port::spawn(&Options::default()).await;
    let pid = rustix::process::Pid::from_raw(port.pid() as i32).expect("pid");
    rustix::process::kill_process(pid, rustix::process::Signal::STOP).expect("stop");
    let pong = port.ping(Duration::from_millis(100)).await;
    println!("SIGSTOP: ping {pong:?} at its 100 ms deadline");
    port.child.start_kill().expect("kill");
    let _ = port.child.wait().await;

    // RLIMIT_AS: an allocation over it aborts the plugin process.
    let mut port = Port::spawn(&Options {
        address_space: Some(512 << 20),
        ..Options::default()
    })
    .await;
    let small = port.call("alloc", r#"{"mib":64}"#, Duration::from_secs(5)).await;
    let big = port.call("alloc", r#"{"mib":1024}"#, Duration::from_secs(5)).await;
    let status = port.child.wait().await.expect("wait");
    println!(
        "RLIMIT_AS 512 MiB: 64 MiB {}, 1 GiB {big:?}, runner {}",
        if matches!(small, Answer::Reply(_)) { "allocated" } else { "failed" },
        status_text(status)
    );
}

async fn sandbox() {
    println!("## sandbox: Landlock in the runner, before the plugin loads");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let open_port = listener.local_addr().expect("addr").port();
    let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let other_port = other.local_addr().expect("addr").port();
    let tmp = std::env::temp_dir().join(format!("s19-{}", std::process::id()));
    for sandboxed in [false, true] {
        let mut port = Port::spawn(&Options {
            sandbox: sandboxed,
            allow_connect: vec![open_port],
            ..Options::default()
        })
        .await;
        println!("sandbox {sandboxed}:");
        let cases = [
            ("read", r#"{"path":"/etc/hostname"}"#.to_string()),
            ("read", format!(r#"{{"path":"{}"}}"#, std::env::current_dir().expect("cwd").join("Cargo.toml").display())),
            ("write", format!(r#"{{"path":"{}"}}"#, tmp.display())),
            ("connect", format!(r#"{{"addr":"127.0.0.1:{open_port}"}}"#)),
            ("connect", format!(r#"{{"addr":"127.0.0.1:{other_port}"}}"#)),
            ("exec", "{}".to_string()),
        ];
        for (tag, payload) in cases {
            let a = port.call(tag, &payload, Duration::from_secs(5)).await;
            let shown = match a {
                Answer::Reply(_) => "ok".to_string(),
                Answer::Error(e) => e,
                other => format!("{other:?}"),
            };
            println!("  {tag} {payload}: {shown}");
        }
        let _ = std::fs::remove_file(&tmp);
        port.stop().await;
    }
    // Cost of the sandbox at start.
    let mut d = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let port = Port::spawn(&Options {
            sandbox: true,
            ..Options::default()
        })
        .await;
        d.push(t.elapsed());
        port.stop().await;
    }
    println!("spawn to ready, sandboxed: {}", summary(d));
}

/// This process's cgroup directory, from `/proc/self/cgroup` (v2 only).
fn own_cgroup() -> Option<PathBuf> {
    let line = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = line.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/')))
}

async fn cgroup() {
    println!("## cgroup: memory.max and pids.max for the runner (cgroup v2)");
    let Some(own) = own_cgroup() else {
        println!("no cgroup v2");
        return;
    };
    println!("own cgroup: {}", own.display());
    // cgroup v2 allows processes only in leaves once controllers are
    // enabled for children: move this process into `host/` first.
    let host = own.join("host");
    let plugin = own.join("plugin");
    let step = |what: &str, r: std::io::Result<()>| match r {
        Ok(()) => true,
        Err(e) => {
            println!("{what}: {e}");
            false
        }
    };
    let ok = step("mkdir host", std::fs::create_dir_all(&host))
        && step(
            "move self to host",
            std::fs::write(host.join("cgroup.procs"), std::process::id().to_string()),
        )
        && step(
            "enable memory and pids",
            std::fs::write(own.join("cgroup.subtree_control"), "+memory +pids"),
        )
        && step("mkdir plugin", std::fs::create_dir_all(&plugin))
        && step("memory.max", std::fs::write(plugin.join("memory.max"), "128M"))
        && step("memory.swap.max", std::fs::write(plugin.join("memory.swap.max"), "0"))
        && step("pids.max", std::fs::write(plugin.join("pids.max"), "16"));
    if !ok {
        println!("(run under a delegated cgroup: systemd-run --user --scope -p Delegate=yes ...)");
        return;
    }
    let mut port = Port::spawn(&Options {
        cgroup: Some(plugin.clone()),
        ..Options::default()
    })
    .await;
    let procs = std::fs::read_to_string(plugin.join("cgroup.procs")).unwrap_or_default();
    println!(
        "runner {} in plugin cgroup: {}",
        port.pid(),
        procs.split_whitespace().any(|p| p == port.pid().to_string())
    );
    let small = port.call("alloc", r#"{"mib":64}"#, Duration::from_secs(5)).await;
    let big = port.call("alloc", r#"{"mib":256}"#, Duration::from_secs(5)).await;
    let status = port.child.wait().await.expect("wait");
    let events = std::fs::read_to_string(plugin.join("memory.events")).unwrap_or_default();
    let oom = events
        .lines()
        .find(|l| l.starts_with("oom_kill "))
        .unwrap_or("oom_kill ?");
    println!(
        "memory.max 128M: 64 MiB {}, 256 MiB {big:?}, runner {}, {oom}",
        if matches!(small, Answer::Reply(_)) { "allocated" } else { "failed" },
        status_text(status)
    );
    let _ = std::fs::remove_dir(&plugin);
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let what = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let all = what == "all";
    if all || what == "start" {
        start().await;
    }
    if all || what == "latency" {
        latency().await;
    }
    if all || what == "throughput" {
        throughput().await;
    }
    if all || what == "failures" {
        failures().await;
    }
    if all || what == "sandbox" {
        sandbox().await;
    }
    if what == "cgroup" {
        cgroup().await;
    }
}
