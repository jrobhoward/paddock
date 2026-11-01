//! Spike S32: a channel between the node and a native app that spins,
//! then sleeps.
//!
//! `s32 bench [BUDGETS_US]` plays the node: a poller thread that serves
//! every app's ring to the node, spinning up to the budget after the last
//! frame before it sleeps on epoll (one eventfd per app), and a tokio
//! runtime of 4 workers for the instance tasks. Each app (`s32 app ...`,
//! this binary again) maps its memfd and waits for the node the same way,
//! spinning up to the budget before it sleeps on a futex in the mapping.
//!
//! Two node paths: `direct`, the poller answers each frame itself (T2's
//! shape), and `task`, the poller hands each frame to the instance's tokio
//! task, which answers (today's structure, one hand-off more).

mod ring;

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::RawFd;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use ring::{RING, Ring, drain, futex_wait, futex_wake, ring_bell};

// Frames, app to node.
const REQ_PID: u8 = 1;
const REQ_GET: u8 = 2;
const CALL: u8 = 3;
const REPLY: u8 = 4;
// Node to app.
const ANSWER: u8 = 10;
const INCOMING: u8 = 11;
const RESULT: u8 = 12;
const GO: u8 = 13;

/// The caller id of a call the node makes.
const NODE: u32 = u32::MAX;
const KEYS: usize = 100_000;
const WARM: usize = 2_000;

/// How a waiting thread spins: a pause instruction, `sched_yield`, or a
/// pause with a `sched_yield` every 32nd turn (the poller never yields
/// in that mode: every app waits on it).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Pause,
    Yield,
    Hybrid,
}

#[derive(Clone, Copy, Debug)]
struct Spin {
    budget: Duration,
    mode: Mode,
}

impl Spin {
    fn arg(&self) -> String {
        format!("{}:{}", self.budget.as_nanos(), self.mode as u8)
    }
    fn parse(s: &str) -> Self {
        let (b, m) = s.split_once(':').unwrap();
        Self {
            budget: Duration::from_nanos(b.parse().unwrap()),
            mode: [Mode::Pause, Mode::Yield, Mode::Hybrid][m.parse::<usize>().unwrap()],
        }
    }
    fn pause(&self, turn: u32, poller: bool) {
        match self.mode {
            Mode::Yield => std::thread::yield_now(),
            Mode::Hybrid if !poller && turn % 32 == 31 => std::thread::yield_now(),
            _ => std::hint::spin_loop(),
        }
    }
}

fn mode_arg(a: Option<&String>) -> Mode {
    match a.map(String::as_str) {
        Some("yield") => Mode::Yield,
        Some("hybrid") => Mode::Hybrid,
        _ => Mode::Pause,
    }
}

fn key(i: usize) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

/// Map `len` bytes of `fd`, shared.
fn map(fd: RawFd, len: usize) -> *mut u8 {
    // SAFETY: a fresh shared mapping of a memfd at least `len` long.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    assert_ne!(p, libc::MAP_FAILED);
    p.cast()
}

fn cpu_self_us() -> u64 {
    // SAFETY: getrusage into a zeroed struct.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let us = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    us(ru.ru_utime) + us(ru.ru_stime)
}

fn cpu_pid_us(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .map(|(_, r)| r.split_whitespace().collect())
        .unwrap_or_default();
    // utime and stime are fields 14 and 15; after ')' they are 12 and 13.
    let ticks: u64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
        + fields.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    ticks * 1_000_000 / hz
}

// ---------------------------------------------------------------- app side

struct AppSide {
    h2c: Ring,
    c2h: Ring,
    bell: RawFd,
    spin: Spin,
    buf: Vec<u8>,
}

impl AppSide {
    fn send(&self, frame: &[u8]) {
        if self.c2h.push(frame) {
            ring_bell(self.bell);
        }
    }

    /// The next frame from the node: spin up to the budget, then sleep.
    fn next(&mut self) -> &[u8] {
        loop {
            if self.h2c.pop(&mut self.buf).unwrap() {
                return &self.buf;
            }
            if !self.spin.budget.is_zero() {
                let start = Instant::now();
                let mut i = 0u32;
                while self.h2c.is_empty() {
                    self.spin.pause(i, false);
                    i += 1;
                    if i % 32 == 0 && start.elapsed() >= self.spin.budget {
                        break;
                    }
                }
                if !self.h2c.is_empty() {
                    continue;
                }
            }
            if self.h2c.sleep() {
                futex_wait(self.h2c.waiting());
            }
        }
    }
}

fn percentile(sorted: &[u32], p: f64) -> u32 {
    sorted[((sorted.len() - 1) as f64 * p) as usize]
}

/// Time `n` rounds of `round` after a warm-up; print ops, wall ns, p50 ns,
/// p99 ns and this process's CPU µs over the warm-up and the rounds.
fn timed(app: &mut AppSide, n: usize, mut round: impl FnMut(&mut AppSide, usize)) {
    let cpu = cpu_self_us();
    for i in 0..WARM {
        round(app, i);
    }
    let mut ns = Vec::with_capacity(n);
    let start = Instant::now();
    for i in 0..n {
        let t = Instant::now();
        round(app, i);
        ns.push(t.elapsed().as_nanos() as u32);
    }
    let wall = start.elapsed().as_nanos();
    let cpu = cpu_self_us() - cpu;
    ns.sort_unstable();
    println!(
        "{n} {wall} {} {} {cpu}",
        percentile(&ns, 0.5),
        percentile(&ns, 0.99)
    );
}

fn app(args: &[String]) {
    // S32_APP_NICE: the app runs this much nicer than the node, so a
    // spinning app never takes a core from the node's poller.
    if let Some(n) = std::env::var("S32_APP_NICE").ok().and_then(|v| v.parse::<i32>().ok()) {
        // SAFETY: setpriority on this process.
        let now = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, now + n) };
    }
    let memfd: RawFd = args[0].parse().unwrap();
    let bell: RawFd = args[1].parse().unwrap();
    let spin = Spin::parse(&args[2]);
    let base = map(memfd, 2 * RING);
    // SAFETY: the mapping lives as long as the process.
    let mut a = AppSide {
        h2c: unsafe { Ring::new(base) },
        c2h: unsafe { Ring::new(base.add(RING)) },
        bell,
        spin,
        buf: Vec::new(),
    };
    while a.next()[0] != GO {}
    let mode = args[3].as_str();
    match mode {
        "pid" | "get" => {
            let n: usize = args[4].parse().unwrap();
            let mut x = 0x9e37_79b9_7f4a_7c15u64;
            timed(&mut a, n, |a, _| {
                if mode == "pid" {
                    a.send(&[REQ_PID]);
                } else {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let mut f = vec![REQ_GET];
                    f.extend(key(x as usize % KEYS));
                    a.send(&f);
                }
                assert_eq!(a.next()[0], ANSWER);
            });
        }
        "caller" => {
            let target: u32 = args[4].parse().unwrap();
            let n: usize = args[5].parse().unwrap();
            let mut f = vec![CALL];
            f.extend(target.to_le_bytes());
            f.extend(b"x");
            timed(&mut a, n, |a, _| {
                a.send(&f);
                assert_eq!(a.next()[0], RESULT);
            });
        }
        "serve" => loop {
            let f = a.next();
            assert_eq!(f[0], INCOMING);
            let mut r = vec![REPLY];
            r.extend(&f[1..5]);
            r.extend(&f[5..]);
            a.send(&r);
        },
        other => panic!("unknown mode {other}"),
    }
}

// --------------------------------------------------------------- node side

struct Inst {
    h2c: Ring,
    c2h: Ring,
    /// One producer at a time into the app's ring.
    h2c_lock: Mutex<()>,
    memfd: RawFd,
    bell: RawFd,
    tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
}

impl Inst {
    fn new() -> Self {
        // Inherited by the app: no CLOEXEC on either.
        // SAFETY: plain system calls; the results are checked.
        let memfd = unsafe { libc::memfd_create(c"s32-rings".as_ptr(), 0) };
        assert!(memfd >= 0);
        assert_eq!(unsafe { libc::ftruncate(memfd, (2 * RING) as i64) }, 0);
        let bell = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
        assert!(bell >= 0);
        let base = map(memfd, 2 * RING);
        // SAFETY: the mapping is never unmapped while the bench runs.
        Self {
            h2c: unsafe { Ring::new(base) },
            c2h: unsafe { Ring::new(base.add(RING)) },
            h2c_lock: Mutex::new(()),
            memfd,
            bell,
            tx: Mutex::new(None),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Path {
    /// The poller answers each frame itself.
    Direct,
    /// The poller hands each frame to the instance's task on the node's
    /// multi-threaded runtime.
    Task,
    /// The poller is a task on a current-thread runtime of its own, which
    /// it keeps spinning with `yield_now`; the instance tasks run there
    /// too, so a hand-off is a queue push on the same thread.
    Local,
}

struct Node {
    insts: Vec<Arc<Inst>>,
    map: RwLock<HashMap<Vec<u8>, Vec<u8>>>,
    pending: Mutex<Option<tokio::sync::oneshot::Sender<Vec<u8>>>>,
}

impl Node {
    fn send(&self, to: usize, frame: &[u8]) {
        let inst = &self.insts[to];
        let _one = inst.h2c_lock.lock().unwrap();
        if inst.h2c.push(frame) {
            futex_wake(inst.h2c.waiting());
        }
    }

    fn handle(&self, from: usize, f: &[u8]) {
        match f[0] {
            REQ_PID => {
                let mut r = vec![ANSWER];
                r.extend((from as u64).to_le_bytes());
                self.send(from, &r);
            }
            REQ_GET => {
                let mut r = vec![ANSWER];
                if let Some(v) = self.map.read().unwrap().get(&f[1..]) {
                    r.extend(v);
                }
                self.send(from, &r);
            }
            CALL => {
                let to = u32::from_le_bytes(f[1..5].try_into().unwrap()) as usize;
                let mut r = vec![INCOMING];
                r.extend((from as u32).to_le_bytes());
                r.extend(&f[5..]);
                self.send(to, &r);
            }
            REPLY => {
                let to = u32::from_le_bytes(f[1..5].try_into().unwrap());
                if to == NODE {
                    if let Some(tx) = self.pending.lock().unwrap().take() {
                        let _ = tx.send(f[5..].to_vec());
                    }
                } else {
                    let mut r = vec![RESULT];
                    r.extend(&f[5..]);
                    self.send(to as usize, &r);
                }
            }
            other => panic!("unknown frame {other}"),
        }
    }
}

/// The node's poller: every app's ring to the node, spinning up to the
/// budget after the last frame, then asleep on epoll.
fn poller(node: Arc<Node>, path: Path, spin: Spin, stop: Arc<AtomicBool>) {
    // SAFETY: plain system calls; the results are checked.
    let ep = unsafe { libc::epoll_create1(0) };
    assert!(ep >= 0);
    for (i, inst) in node.insts.iter().enumerate() {
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: i as u64,
        };
        assert_eq!(
            unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, inst.bell, &mut ev) },
            0
        );
    }
    let mut buf = Vec::new();
    let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 64];
    let mut last = Instant::now();
    let mut idle = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let mut found = false;
        for (i, inst) in node.insts.iter().enumerate() {
            while inst.c2h.pop(&mut buf).expect("corrupt ring: end the app") {
                found = true;
                match path {
                    Path::Direct => node.handle(i, &buf),
                    Path::Task | Path::Local => {
                        let tx = inst.tx.lock().unwrap();
                        tx.as_ref().unwrap().send(buf.clone()).unwrap();
                    }
                }
            }
        }
        if found {
            idle = 0;
            continue;
        }
        idle += 1;
        if idle == 1 {
            last = Instant::now();
        }
        if !spin.budget.is_zero() && (idle % 16 != 0 || last.elapsed() < spin.budget) {
            spin.pause(idle, true);
            continue;
        }
        // Sleep: every ring says so, unless one has a frame by now.
        let busy = !node.insts.iter().all(|inst| inst.c2h.sleep());
        if !busy {
            // SAFETY: a valid buffer of 64 events.
            let n = unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), 64, 50) };
            for ev in events.iter().take(n.max(0) as usize) {
                drain(node.insts[ev.u64 as usize].bell);
            }
        }
        for inst in &node.insts {
            inst.c2h.waiting().store(0, Ordering::SeqCst);
        }
        idle = 0;
    }
    // SAFETY: closing our own epoll fd.
    unsafe { libc::close(ep) };
}

struct Epoll(RawFd);

impl std::os::fd::AsRawFd for Epoll {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// `Path::Local`: the poller and the instance tasks on one current-thread
/// runtime, the poller spinning with `yield_now` up to the budget, then
/// awaiting the epoll fd (every app's eventfd) through the runtime's I/O
/// driver, so the thread parks only when every task waits.
fn local_poller(node: Arc<Node>, spin: Spin, stop: Arc<AtomicBool>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        for (i, inst) in node.insts.iter().enumerate() {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            *inst.tx.lock().unwrap() = Some(tx);
            let node = node.clone();
            tokio::spawn(async move {
                while let Some(f) = rx.recv().await {
                    node.handle(i, &f);
                }
            });
        }
        // SAFETY: plain system calls; the results are checked.
        let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        assert!(ep >= 0);
        for (i, inst) in node.insts.iter().enumerate() {
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: i as u64,
            };
            assert_eq!(
                unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, inst.bell, &mut ev) },
                0
            );
        }
        #[allow(deprecated)] // a spike: the fd outlives the AsyncFd
        let ready = tokio::io::unix::AsyncFd::new(Epoll(ep)).unwrap();
        let mut buf = Vec::new();
        let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 64];
        let mut idle = 0u32;
        let mut last = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let mut found = false;
            for inst in &node.insts {
                while inst.c2h.pop(&mut buf).expect("corrupt ring: end the app") {
                    found = true;
                    let tx = inst.tx.lock().unwrap();
                    tx.as_ref().unwrap().send(buf.clone()).unwrap();
                }
            }
            if found {
                idle = 0;
                tokio::task::yield_now().await;
                continue;
            }
            idle += 1;
            if idle == 1 {
                last = Instant::now();
            }
            if !spin.budget.is_zero() && (idle % 16 != 0 || last.elapsed() < spin.budget) {
                tokio::task::yield_now().await;
                continue;
            }
            let busy = !node.insts.iter().all(|inst| inst.c2h.sleep());
            if !busy {
                if let Ok(Ok(mut guard)) =
                    tokio::time::timeout(Duration::from_millis(50), ready.readable()).await
                {
                    guard.clear_ready();
                }
                // SAFETY: a valid buffer of 64 events; no wait.
                let n = unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), 64, 0) };
                for ev in events.iter().take(n.max(0) as usize) {
                    drain(node.insts[ev.u64 as usize].bell);
                }
            }
            for inst in &node.insts {
                inst.c2h.waiting().store(0, Ordering::SeqCst);
            }
            idle = 0;
        }
        drop(ready);
        // SAFETY: closing our own epoll fd.
        unsafe { libc::close(ep) };
    });
}

/// Apps by mode, started under one poller; the result lines of the apps
/// that print one, the node's CPU µs over the measured part, and the apps'.
struct Run {
    lines: Vec<String>,
    node_cpu_us: u64,
    apps_cpu_us: u64,
    rtt: Vec<u32>,
}

/// Start `modes.len()` apps, release them together and wait for the ones
/// that finish (`serve` apps are killed after `driver`).
fn run(
    rt: &tokio::runtime::Runtime,
    map: &Arc<RwLock<HashMap<Vec<u8>, Vec<u8>>>>,
    path: Path,
    spin: Spin,
    modes: &[Vec<String>],
    driver: Option<usize>,
    idle_for: Option<Duration>,
) -> Run {
    let insts: Vec<Arc<Inst>> = modes.iter().map(|_| Arc::new(Inst::new())).collect();
    let node = Arc::new(Node {
        insts: insts.clone(),
        map: RwLock::new(HashMap::new()),
        pending: Mutex::new(None),
    });
    *node.map.write().unwrap() = map.read().unwrap().clone();
    if path == Path::Task {
        for (i, inst) in insts.iter().enumerate() {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            *inst.tx.lock().unwrap() = Some(tx);
            let node = node.clone();
            rt.spawn(async move {
                while let Some(f) = rx.recv().await {
                    node.handle(i, &f);
                }
            });
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let poll = {
        let (node, stop) = (node.clone(), stop.clone());
        if path == Path::Local {
            std::thread::spawn(move || local_poller(node, spin, stop))
        } else {
            std::thread::spawn(move || poller(node, path, spin, stop))
        }
    };
    let exe = std::env::current_exe().unwrap();
    let mut children: Vec<Child> = insts
        .iter()
        .zip(modes)
        .map(|(inst, mode)| {
            Command::new(&exe)
                .arg("app")
                .arg(inst.memfd.to_string())
                .arg(inst.bell.to_string())
                .arg(spin.arg())
                .args(mode)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300 + 5 * modes.len() as u64));

    let node_cpu = cpu_self_us();
    let pids: Vec<u32> = children.iter().map(Child::id).collect();
    let serve_before: u64 = pids
        .iter()
        .zip(modes)
        .filter(|(_, m)| m[0] == "serve")
        .map(|(p, _)| cpu_pid_us(*p))
        .sum();
    for i in 0..insts.len() {
        node.send(i, &[GO]);
    }
    let mut rtt = Vec::new();
    if let Some(to) = driver {
        // A tokio task calls app `to` one call at a time, as an instance
        // task would; the poller completes each call.
        let node = node.clone();
        rtt = rt.block_on(async move {
            let mut ns = Vec::new();
            for i in 0..WARM + 20_000 {
                let (tx, rx) = tokio::sync::oneshot::channel();
                *node.pending.lock().unwrap() = Some(tx);
                let mut f = vec![INCOMING];
                f.extend(NODE.to_le_bytes());
                f.extend(b"x");
                let t = Instant::now();
                node.send(to, &f);
                rx.await.unwrap();
                if i >= WARM {
                    ns.push(t.elapsed().as_nanos() as u32);
                }
            }
            ns.sort_unstable();
            ns
        });
    }
    if let Some(d) = idle_for {
        std::thread::sleep(d);
    }
    let mut lines = Vec::new();
    let mut finished_cpu = 0;
    for (c, mode) in children.iter_mut().zip(modes) {
        if mode[0] == "serve" {
            continue;
        }
        let mut out = String::new();
        c.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        c.wait().unwrap();
        let line = out.trim().to_string();
        finished_cpu += line
            .split(' ')
            .nth(4)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        lines.push(line);
    }
    let node_cpu_us = cpu_self_us() - node_cpu;
    // The serving apps' CPU from /proc since GO; the finished ones
    // reported theirs.
    let serving = |p: &&u32| modes[pids.iter().position(|x| x == *p).unwrap()][0] == "serve";
    let apps_after: u64 = pids.iter().filter(serving).map(|p| cpu_pid_us(*p)).sum();
    for c in &mut children {
        let _ = c.kill();
        let _ = c.wait();
    }
    stop.store(true, Ordering::Relaxed);
    for inst in &insts {
        ring_bell(inst.bell);
    }
    poll.join().unwrap();
    for inst in &insts {
        *inst.tx.lock().unwrap() = None;
        // SAFETY: closing our own descriptors; the mapping stays (leaked).
        unsafe {
            libc::close(inst.memfd);
            libc::close(inst.bell);
        }
    }
    Run {
        lines,
        node_cpu_us,
        apps_cpu_us: finished_cpu + apps_after.saturating_sub(serve_before),
        rtt,
    }
}

fn parse(line: &str) -> (f64, f64, f64, f64) {
    let v: Vec<f64> = line.split(' ').map(|x| x.parse().unwrap()).collect();
    (v[0], v[1], v[2], v[3])
}

fn modes(m: &[&str]) -> Vec<String> {
    m.iter().map(|s| s.to_string()).collect()
}

fn bench(args: &[String]) {
    let budgets: Vec<u64> = args
        .first()
        .map(|s| s.split(',').map(|b| b.parse().unwrap()).collect())
        .unwrap_or_else(|| vec![0, 2, 5, 20, 50]);
    // "none" skips the concurrent cases.
    let concurrency: Vec<usize> = args
        .get(1)
        .filter(|s| *s != "none")
        .map(|s| s.split(',').map(|b| b.parse().unwrap()).collect())
        .unwrap_or_else(|| if args.get(1).is_some() { vec![] } else { vec![16, 64] });
    // How the sequential cases spin: pause (default), yield or hybrid;
    // then which modes the concurrent cases try (default all three).
    let seq_mode = mode_arg(args.get(2));
    let conc_modes: Vec<Mode> = args.get(3).map_or_else(
        || vec![Mode::Pause, Mode::Yield, Mode::Hybrid],
        |a| a.split(',').map(|m| mode_arg(Some(&m.to_string()))).collect(),
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let map: HashMap<Vec<u8>, Vec<u8>> = (0..KEYS).map(|i| (key(i), vec![b'v'; 16])).collect();
    let map = Arc::new(RwLock::new(map));
    let n = 20_000usize;
    let ns = n.to_string();

    println!(
        "# {} cores; budgets {:?} µs; {n} sequential rounds after {WARM}, spinning with {}; p50 / p99 µs; CPU µs per round (node + apps)",
        std::thread::available_parallelism().unwrap(),
        budgets,
        format!("{seq_mode:?}")
    );
    println!("| budget | case | path | p50 | p99 | CPU per round |");
    println!("| --- | --- | --- | --- | --- | --- |");
    for &b in &budgets {
        let spin = Spin {
            budget: Duration::from_micros(b),
            mode: seq_mode,
        };
        for path in [Path::Direct, Path::Task, Path::Local] {
            for (case, m) in [
                ("request (pid)", vec![modes(&["pid", &ns])]),
                ("bucket get", vec![modes(&["get", &ns])]),
                (
                    "call between instances",
                    vec![modes(&["caller", "1", &ns]), modes(&["serve"])],
                ),
            ] {
                let r = run(&rt, &map, path, spin, &m, None, None);
                let (ops, _, p50, p99) = parse(&r.lines[0]);
                println!(
                    "| {b} | {case} | {path:?} | {:.2} | {:.2} | {:.2} |",
                    p50 / 1e3,
                    p99 / 1e3,
                    (r.node_cpu_us + r.apps_cpu_us) as f64 / (ops + WARM as f64)
                );
            }
        }
        let r = run(&rt, &map, Path::Direct, spin, &[modes(&["serve"])], Some(0), None);
        println!(
            "| {b} | call from the node (tokio task) | Direct | {:.2} | {:.2} | {:.2} |",
            percentile(&r.rtt, 0.5) as f64 / 1e3,
            percentile(&r.rtt, 0.99) as f64 / 1e3,
            (r.node_cpu_us + r.apps_cpu_us) as f64 / (r.rtt.len() + WARM) as f64
        );
    }

    println!();
    println!("Concurrent apps, each {n} requests (pid): requests/s, p50 of the apps' p50s and the worst p99 (µs), CPU µs per request");
    println!("| budget | spin | path | apps | requests/s | p50 | worst p99 | CPU per request |");
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    for &b in budgets.iter().filter(|_| !concurrency.is_empty()) {
        for (mode, path) in conc_modes
            .iter()
            .flat_map(|m| [(*m, Path::Direct), (*m, Path::Local)])
        {
            if b == 0 && mode != Mode::Pause {
                continue;
            }
            let spin = Spin {
                budget: Duration::from_micros(b),
                mode,
            };
            for &k in &concurrency {
                let m: Vec<Vec<String>> = (0..k).map(|_| modes(&["pid", &ns])).collect();
                let r = run(&rt, &map, path, spin, &m, None, None);
                let parsed: Vec<_> = r.lines.iter().map(|l| parse(l)).collect();
                let wall = parsed.iter().map(|p| p.1).fold(0.0, f64::max) / 1e9;
                let ops: f64 = parsed.iter().map(|p| p.0).sum();
                let mut p50s: Vec<f64> = parsed.iter().map(|p| p.2).collect();
                p50s.sort_by(f64::total_cmp);
                let worst = parsed.iter().map(|p| p.3).fold(0.0, f64::max);
                println!(
                    "| {b} | {} | {path:?} | {k} | {:.0} | {:.2} | {:.1} | {:.2} |",
                    format!("{mode:?}"),
                    ops / wall,
                    p50s[p50s.len() / 2] / 1e3,
                    worst / 1e3,
                    (r.node_cpu_us + r.apps_cpu_us) as f64 / (ops + (k * WARM) as f64)
                );
            }
        }
    }

    println!();
    println!("Idle: 100 apps waiting, CPU over 2 s after the last frame (ms, node + apps)");
    for &b in &budgets {
        let spin = Spin {
            budget: Duration::from_micros(b),
            mode: seq_mode,
        };
        for path in [Path::Direct, Path::Local] {
            let m: Vec<Vec<String>> = (0..100).map(|_| modes(&["serve"])).collect();
            let r = run(&rt, &map, path, spin, &m, None, Some(Duration::from_secs(2)));
            println!(
                "budget {b} µs, {path:?}: {:.1} ms",
                (r.node_cpu_us + r.apps_cpu_us) as f64 / 1e3
            );
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("app") => app(&args[1..]),
        Some("bench") => bench(&args[1..]),
        _ => eprintln!("usage: s32 bench [BUDGETS_US] [APPS] | s32 app ..."),
    }
}
