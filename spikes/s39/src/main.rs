//! Spike S39: sends from an app's own threads.
//!
//! `s39 bench ...` plays the node: one poller thread that sweeps every
//! ring to the node and answers each frame itself (S32's direct path),
//! spinning up to the budget after the last frame before it sleeps on
//! epoll over the apps' bells. It starts `s39 app ...` (this binary again)
//! as the subject app, on the mechanism under test, and, for calls, an
//! echo app whose callback thread answers them. See `app.rs` for the
//! mechanisms and README.md for the runs.

mod app;
mod wire;

use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};

use bulkhead_native::channel::{Channel, Spin, bell, door, take_waiting, wake_reader};

use app::{Chan, Mech, PerThread, Queue, Shared};
use wire::{
    ANSWER, CALL, CAST, DONE, GET, INCOMING, PAYLOAD, REPLY, START, TICK, TICK_DONE, frame, id,
    kind, read_frame, rest, u32_at, write_frame,
};

const KEYS: u32 = 100_000;

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
    let ticks: u64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
        + fields.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    ticks * 1_000_000 / hz
}

// --------------------------------------------------------------- node

struct NodeChan {
    channel: Channel,
    door: OwnedFd,
}

struct NodeApp {
    chans: Vec<NodeChan>,
    bell: OwnedFd,
    pid: u32,
}

/// What the node measured between a subject's START and DONE.
struct NodeResult {
    node_cpu_us: u64,
    others_cpu_us: u64,
    casts: u64,
}

struct Node {
    apps: Vec<NodeApp>,
    values: Vec<[u8; PAYLOAD]>,
    casts: u64,
    start: Option<(u64, u64)>,
    done: mpsc::Sender<NodeResult>,
}

impl Node {
    fn send(&self, app: usize, chan: usize, f: &[u8]) {
        let c = &self.apps[app].chans[chan];
        let r = c.channel.to_app();
        write_frame(r, f, || wake_reader(r.reader_waiting(), c.door.as_fd()));
    }

    fn others_cpu(&self, but: usize) -> u64 {
        self.apps
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != but)
            .map(|(_, a)| cpu_pid_us(a.pid))
            .sum()
    }

    fn handle(&mut self, app: usize, chan: usize, body: &[u8]) {
        let i = id(body);
        match kind(body) {
            GET => {
                let key = u32_at(rest(body), 0) % KEYS;
                let v = self.values[key as usize];
                self.send(app, chan, &frame(ANSWER, i, &v));
            }
            CAST => self.casts += 1,
            CALL => {
                let r = rest(body);
                let to = u32_at(r, 0) as usize;
                let mut inc = Vec::with_capacity(8 + PAYLOAD);
                inc.extend((app as u32).to_le_bytes());
                inc.extend((chan as u32).to_le_bytes());
                inc.extend(&r[4..]);
                self.send(to, 0, &frame(INCOMING, i, &inc));
            }
            REPLY => {
                let r = rest(body);
                let (to, to_chan) = (u32_at(r, 0) as usize, u32_at(r, 4) as usize);
                self.send(to, to_chan, &frame(ANSWER, i, &r[8..]));
            }
            START => {
                self.casts = 0;
                self.start = Some((cpu_self_us(), self.others_cpu(app)));
                self.send(app, chan, &frame(ANSWER, i, &[]));
            }
            DONE => {
                let (node0, others0) = self.start.take().unwrap();
                let _ = self.done.send(NodeResult {
                    node_cpu_us: cpu_self_us() - node0,
                    others_cpu_us: self.others_cpu(app) - others0,
                    casts: self.casts,
                });
                self.send(app, chan, &frame(ANSWER, i, &[]));
            }
            TICK_DONE => self.send(app, chan, &frame(TICK, 0, &[])),
            k => panic!("frame of kind {k}"),
        }
    }
}

/// Sweep every ring to the node, answering each frame; spin up to the
/// budget after the last, then mark every ring and sleep on the bells.
fn poller(mut node: Node, spin: Duration, stop: Arc<AtomicBool>) {
    // SAFETY: plain system calls; results checked.
    let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    assert!(ep >= 0);
    for (i, a) in node.apps.iter().enumerate() {
        rustix::fs::fcntl_setfl(&a.bell, rustix::fs::OFlags::NONBLOCK).unwrap();
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: i as u64,
        };
        assert_eq!(
            unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, a.bell.as_raw_fd(), &mut ev) },
            0
        );
    }
    let mut buf = Vec::new();
    let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 64];
    let mut last = Instant::now();
    let mut idle = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let mut found = false;
        for a in 0..node.apps.len() {
            for c in 0..node.apps[a].chans.len() {
                while read_frame(node.apps[a].chans[c].channel.to_node(), &mut buf) {
                    found = true;
                    node.handle(a, c, &buf);
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
        if !spin.is_zero() && (idle % 16 != 0 || last.elapsed() < spin) {
            std::thread::yield_now();
            continue;
        }
        let rings = || node.apps.iter().flat_map(|a| a.chans.iter());
        for c in rings() {
            c.channel
                .to_node()
                .reader_waiting()
                .store(1, Ordering::SeqCst);
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        if !rings().any(|c| c.channel.to_node().is_readable()) {
            // SAFETY: a valid buffer of 64 events.
            let n = unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), 64, 50) };
            for ev in events.iter().take(n.max(0) as usize) {
                let a = &node.apps[ev.u64 as usize];
                if matches!(rustix::io::read(&a.bell, &mut [0u8; 256][..]), Ok(0)) {
                    // The app has gone.
                    unsafe {
                        libc::epoll_ctl(
                            ep,
                            libc::EPOLL_CTL_DEL,
                            a.bell.as_raw_fd(),
                            std::ptr::null_mut(),
                        )
                    };
                }
            }
        }
        for c in rings() {
            let _ = take_waiting(c.channel.to_node().reader_waiting());
        }
        idle = 0;
    }
    // SAFETY: our own epoll fd.
    unsafe { libc::close(ep) };
}

// --------------------------------------------------------------- bench

#[derive(Clone, Debug)]
struct RunSpec {
    mech: String,
    threads: usize,
    op: String,
    busy_us: u64,
    n: usize,
    warm: usize,
    spin_us: u64,
}

/// Make an app's channels and spawn it with `args`; the node's side.
fn spawn_app(
    role: &str,
    spec: &RunSpec,
    chans: usize,
    self_wake: bool,
) -> (NodeApp, std::process::Child) {
    let mut node_chans = Vec::new();
    let mut child_fds: Vec<OwnedFd> = Vec::new();
    let mut spec_arg = Vec::new();
    for _ in 0..chans {
        let (channel, memfd) = Channel::create().unwrap();
        let (door_r, door_w) = door().unwrap();
        spec_arg.push(format!("{}:{}", memfd.as_raw_fd(), door_r.as_raw_fd()));
        child_fds.push(memfd);
        child_fds.push(door_r);
        node_chans.push(NodeChan {
            channel,
            door: door_w,
        });
    }
    let (bell_r, bell_w) = bell().unwrap();
    let bell_arg = bell_w.as_raw_fd();
    child_fds.push(bell_w);
    let wake_arg = if self_wake {
        let w = node_chans[0].door.try_clone().unwrap();
        let n = w.as_raw_fd();
        child_fds.push(w);
        n
    } else {
        -1
    };
    for fd in &child_fds {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty()).unwrap();
    }
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "app".to_string(),
            role.to_string(),
            spec.mech.clone(),
            spec.threads.to_string(),
            spec.op.clone(),
            spec.busy_us.to_string(),
            spec.n.to_string(),
            spec.warm.to_string(),
            spec.spin_us.to_string(),
            bell_arg.to_string(),
            wake_arg.to_string(),
            spec_arg.join(","),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child_fds);
    let pid = child.id();
    (
        NodeApp {
            chans: node_chans,
            bell: bell_r,
            pid,
        },
        child,
    )
}

/// One run; its result line.
fn run(spec: &RunSpec) -> String {
    let chans = if spec.mech == "m1" { spec.threads + 1 } else { 1 };
    let (subject, mut child) = spawn_app("subject", spec, chans, spec.mech == "m3");
    let mut apps = vec![subject];
    let mut echo = None;
    if spec.op == "call" {
        let (e, c) = spawn_app("echo", spec, 1, false);
        apps.push(e);
        echo = Some(c);
    }
    let (tx, rx) = mpsc::channel();
    let node = Node {
        apps,
        values: (0..KEYS)
            .map(|k| {
                let mut v = [0u8; PAYLOAD];
                v[..4].copy_from_slice(&k.to_le_bytes());
                v
            })
            .collect(),
        casts: 0,
        start: None,
        done: tx,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let poll = {
        let stop = stop.clone();
        let spin = Duration::from_micros(spec.spin_us);
        std::thread::spawn(move || poller(node, spin, stop))
    };
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    let status = child.wait().unwrap();
    let nr = rx.recv_timeout(Duration::from_secs(5));
    stop.store(true, Ordering::Relaxed);
    poll.join().unwrap();
    if let Some(mut e) = echo {
        let _ = e.kill();
        let _ = e.wait();
    }
    let Ok(nr) = nr else {
        return format!("{spec:?} FAILED: {status} {out}");
    };
    // app: ops wall_ns p50_ns p99_ns app_cpu_us
    let f: Vec<f64> = out.split_whitespace().map(|v| v.parse().unwrap()).collect();
    let (ops, wall_ns, p50, p99, app_cpu) = (f[0], f[1], f[2], f[3], f[4]);
    if spec.op == "cast" {
        assert_eq!(nr.casts as f64, ops, "casts lost");
    }
    let per = |us: f64| us / ops;
    let node_cpu = nr.node_cpu_us as f64;
    let others = nr.others_cpu_us as f64;
    format!(
        "{:<4} t={} {:<4} busy={:>4}us  {:>9.0} ops/s  p50 {:>7.2} us  p99 {:>8.2} us  cpu/op node {:>5.2} app {:>5.2} echo {:>5.2} total {:>5.2} us",
        spec.mech,
        spec.threads,
        spec.op,
        spec.busy_us,
        ops / (wall_ns / 1e9),
        p50 / 1e3,
        p99 / 1e3,
        per(node_cpu),
        per(app_cpu),
        per(others),
        per(node_cpu + app_cpu + others),
    )
}

fn list<T: std::str::FromStr>(s: &str) -> Vec<T>
where
    T::Err: std::fmt::Debug,
{
    s.split(',').map(|v| v.parse().unwrap()).collect()
}

fn bench(args: &[String]) {
    let mut mechs: Vec<String> = list("m0,m1,m2,m2r,m3");
    let mut threads: Vec<usize> = list("1,2,4,8");
    let mut ops: Vec<String> = list("get,cast,call");
    let mut busy: Vec<u64> = list("0,1000");
    let mut n = 20_000;
    let mut warm = 2_000;
    let mut spin_us = 20;
    let mut rounds = 1;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let v = it.next().expect("a value");
        match a.as_str() {
            "--mechs" => mechs = list(v),
            "--threads" => threads = list(v),
            "--ops" => ops = list(v),
            "--busy" => busy = list(v),
            "--n" => n = v.parse().unwrap(),
            "--warm" => warm = v.parse().unwrap(),
            "--spin" => spin_us = v.parse().unwrap(),
            "--rounds" => rounds = v.parse().unwrap(),
            other => panic!("unknown flag {other}"),
        }
    }
    for round in 0..rounds {
        for op in &ops {
            for &b in &busy {
                for &t in &threads {
                    for m in &mechs {
                        // m0 is one thread with no callback thread.
                        if m == "m0" && (t != 1 || b != 0) {
                            continue;
                        }
                        let spec = RunSpec {
                            mech: m.clone(),
                            threads: t,
                            op: op.clone(),
                            busy_us: b,
                            n,
                            warm,
                            spin_us,
                        };
                        println!("r{round} {}", run(&spec));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- app

fn app(args: &[String]) {
    let role = args[0].as_str();
    let mech = args[1].as_str();
    let threads: usize = args[2].parse().unwrap();
    let op = args[3].clone();
    let busy = Duration::from_micros(args[4].parse().unwrap());
    let n: usize = args[5].parse().unwrap();
    let warm: usize = args[6].parse().unwrap();
    let spin = Spin(Duration::from_micros(args[7].parse().unwrap()));
    // SAFETY: the node put these descriptors here for this process.
    let own = |n: i32| unsafe { OwnedFd::from_raw_fd(n) };
    let bell = Arc::new(own(args[8].parse().unwrap()));
    let wake: i32 = args[9].parse().unwrap();
    let mut chans: Vec<Chan> = args[10]
        .split(',')
        .map(|s| {
            let (m, d) = s.split_once(':').unwrap();
            let memfd = own(m.parse().unwrap());
            Chan {
                channel: Channel::open(memfd.as_fd()).unwrap(),
                door: own(d.parse().unwrap()),
                bell: bell.clone(),
                spin,
            }
        })
        .collect();

    if role == "echo" {
        PerThread {
            chans,
            busy: Duration::ZERO,
        }
        .run_callbacks();
        return;
    }

    let m: Arc<dyn Mech> = match mech {
        "m0" | "m1" => Arc::new(PerThread { chans, busy }),
        "m2" | "m2r" => Shared::new(chans.remove(0), threads, busy, mech == "m2r"),
        "m3" => Arc::new(Queue::new(chans.remove(0), threads, busy, own(wake))),
        other => panic!("unknown mechanism {other}"),
    };
    if mech != "m0" {
        let m = m.clone();
        std::thread::spawn(move || m.run_callbacks());
    }

    let start = Arc::new(Barrier::new(threads + 1));
    let end = Arc::new(Barrier::new(threads + 1));
    let workers: Vec<_> = (1..=threads)
        .map(|t| {
            let (m, start, end, op) = (m.clone(), start.clone(), end.clone(), op.clone());
            std::thread::spawn(move || {
                m.enter(t);
                let mut lat = Vec::with_capacity(n);
                let one = |i: usize| {
                    let key = ((t as u32) * 7919 + i as u32) % KEYS;
                    match op.as_str() {
                        "get" => {
                            let v = m.request(t, GET, &key.to_le_bytes());
                            debug_assert_eq!(&v[..4], &key.to_le_bytes());
                        }
                        "cast" => m.post(t, CAST, &key.to_le_bytes()),
                        "call" => {
                            let mut r = Vec::with_capacity(4 + PAYLOAD);
                            r.extend(1u32.to_le_bytes());
                            r.extend([7u8; PAYLOAD]);
                            m.request(t, CALL, &r);
                        }
                        other => panic!("unknown op {other}"),
                    }
                };
                for i in 0..warm {
                    one(i);
                }
                if op == "cast" {
                    m.request(t, GET, &0u32.to_le_bytes());
                }
                start.wait();
                if t == 1 {
                    m.request(t, START, &[]);
                }
                start.wait();
                let began = Instant::now();
                for i in 0..n {
                    let s = Instant::now();
                    one(i);
                    lat.push(s.elapsed().as_nanos() as u64);
                }
                // Every cast is in the node before the end: a get after
                // them on the same path answers only once they are.
                if op == "cast" {
                    m.request(t, GET, &0u32.to_le_bytes());
                }
                let ended = Instant::now();
                end.wait();
                end.wait();
                if t == 1 {
                    m.request(t, DONE, &[]);
                }
                (lat, began, ended)
            })
        })
        .collect();
    // The CPU reading is taken before the workers are released and the
    // wall time from their own clocks: on 4 cores busy with the run, this
    // thread can be descheduled until they are nearly done.
    start.wait();
    let cpu0 = cpu_self_us();
    let burned0 = app::BURNED_US.load(std::sync::atomic::Ordering::SeqCst);
    start.wait();
    end.wait();
    // Less the busy callbacks' own CPU: only what reaching the node costs.
    let burned = app::BURNED_US.load(std::sync::atomic::Ordering::SeqCst) - burned0;
    let cpu = (cpu_self_us() - cpu0).saturating_sub(burned);
    end.wait();
    let done: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    let began = done.iter().map(|d| d.1).min().unwrap();
    let ended = done.iter().map(|d| d.2).max().unwrap();
    let wall = (ended - began).as_nanos();
    let mut lat: Vec<u64> = done.into_iter().flat_map(|d| d.0).collect();
    lat.sort_unstable();
    let pct = |p: f64| lat[((lat.len() - 1) as f64 * p) as usize];
    println!(
        "{} {wall} {} {} {cpu}",
        lat.len(),
        pct(0.5),
        pct(0.99)
    );
    std::process::exit(0);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("bench") => bench(&args[2..]),
        Some("app") => app(&args[2..]),
        _ => eprintln!("usage: s39 bench [--mechs m0,m1,m2,m2r,m3] [--threads 1,2,4,8] [--ops get,cast,call] [--busy 0,1000] [--n N] [--warm N] [--spin US] [--rounds N]"),
    }
}
