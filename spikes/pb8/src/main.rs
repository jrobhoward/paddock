//! Spike PB8: a direct channel between two native apps, against a call
//! through a hub.
//!
//! `pb8 bench [ROUNDS]` plays the node, and `pb8 app ...` (this binary,
//! started by the bench) plays each app. Apps come in pairs: a caller and
//! its target. Three designs carry their frames:
//!
//! - `hub`: each app has S32's two rings to the hub (one memfd), and the
//!   hub's poller threads forward each frame to the other app's ring. No
//!   tasks, no mailboxes: the floor of any design through the node.
//! - `ring`: a memfd of two rings between the pair, which the hub creates,
//!   hands to both and keeps mapped; it revokes the pair by setting a word
//!   both apps check.
//! - `socket`: a Unix `SOCK_SEQPACKET` socketpair the hub hands out,
//!   keeping a dup; it revokes the pair with `shutdown(2)` on it.
//!
//! Every waiting thread spins with `sched_yield` up to the budget (20 µs,
//! Bulkhead's `spin_us`), then sleeps: on a futex in the ring, on the
//! hub's epoll of the apps' eventfds, or in `poll` on the socket.
//!
//! `pb8 revoke` starts a busy pair on each direct design, revokes it, and
//! times how long both apps take to stop.

mod ring;

use std::io::Read;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ring::{RING, Ring, drain, futex_wait, futex_wake, ring_bell};

const CALL: u8 = 1;
const REPLY: u8 = 2;
const CAST: u8 = 3;
/// A cast with this sequence number stops the target.
const STOP: u64 = u64::MAX;
/// A message's body: the size of a small call.
const BODY: usize = 16;
const FRAME: usize = 1 + 8 + BODY;
const WARM: u64 = 2_000;
/// A pair's memfd: its two rings, then the revoked word.
const REVOKED_AT: usize = 2 * RING;
const PAIR_LEN: usize = 2 * RING + 64;
/// The hub's poller threads, as the node's four tokio workers.
const HUB_THREADS: usize = 4;
/// Exit code of an app whose channel was revoked.
const REVOKED: i32 = 4;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Design {
    Hub,
    Ring,
    Socket,
}

impl Design {
    const ALL: [Design; 3] = [Design::Hub, Design::Ring, Design::Socket];

    fn name(self) -> &'static str {
        match self {
            Design::Hub => "hub",
            Design::Ring => "ring",
            Design::Socket => "socket",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "hub" => Design::Hub,
            "ring" => Design::Ring,
            _ => Design::Socket,
        }
    }
}

fn frame(kind: u8, seq: u64, value: u64) -> [u8; FRAME] {
    let mut f = [0u8; FRAME];
    f[0] = kind;
    f[1..9].copy_from_slice(&seq.to_le_bytes());
    f[9..17].copy_from_slice(&value.to_le_bytes());
    f
}

fn field(f: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(f[at..at + 8].try_into().unwrap())
}

fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn cpu_us(who: i32) -> u64 {
    // SAFETY: getrusage into a zeroed struct.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(who, &mut ru) };
    let us = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    us(ru.ru_utime) + us(ru.ru_stime)
}

fn memfd(len: usize) -> RawFd {
    // SAFETY: a name and flags; then ftruncate on the fd just made.
    let fd = unsafe { libc::memfd_create(c"pb8".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    assert_eq!(unsafe { libc::ftruncate(fd, len as i64) }, 0);
    fd
}

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

fn eventfd() -> RawFd {
    // SAFETY: no pointers.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    assert!(fd >= 0);
    fd
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

// ---------------------------------------------------------------- app side

/// How a sender wakes the reader of the ring it wrote.
enum Wake {
    /// The hub's poller sleeps in epoll on this eventfd.
    Bell(RawFd),
    /// The other app sleeps on the ring's futex word.
    Futex,
}

enum Chan {
    Rings {
        rx: Ring,
        tx: Ring,
        wake: Wake,
        revoked: Option<&'static AtomicU32>,
    },
    Socket(RawFd),
}

fn revoked_exit() -> ! {
    std::process::exit(REVOKED)
}

impl Chan {
    fn check(&self) {
        if let Chan::Rings {
            revoked: Some(word),
            ..
        } = self
            && word.load(Ordering::Acquire) == 1
        {
            revoked_exit();
        }
    }

    fn send(&self, f: &[u8]) {
        match self {
            Chan::Rings { tx, wake, .. } => {
                self.check();
                if tx.push(f) {
                    match wake {
                        Wake::Bell(fd) => ring_bell(*fd),
                        Wake::Futex => futex_wake(tx.waiting()),
                    }
                }
            }
            Chan::Socket(fd) => loop {
                // SAFETY: a send from a valid buffer.
                let n = unsafe {
                    libc::send(
                        *fd,
                        f.as_ptr().cast(),
                        f.len(),
                        libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                    )
                };
                if n == f.len() as isize {
                    return;
                }
                match errno() {
                    libc::EAGAIN => poll(*fd, libc::POLLOUT),
                    _ => revoked_exit(),
                }
            },
        }
    }

    /// The next frame, spinning up to `budget` before sleeping.
    fn recv(&self, out: &mut Vec<u8>, budget: Duration) {
        let mut since = Instant::now();
        match self {
            Chan::Rings { rx, .. } => loop {
                self.check();
                match rx.pop(out) {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(_) => std::process::exit(5),
                }
                if since.elapsed() < budget {
                    std::thread::yield_now();
                    continue;
                }
                if rx.sleep() {
                    futex_wait(rx.waiting());
                }
                since = Instant::now();
            },
            Chan::Socket(fd) => loop {
                out.resize(64, 0);
                // SAFETY: a receive into a valid buffer.
                let n = unsafe { libc::recv(*fd, out.as_mut_ptr().cast(), 64, libc::MSG_DONTWAIT) };
                if n > 0 {
                    out.truncate(n as usize);
                    return;
                }
                if n == 0 || errno() != libc::EAGAIN {
                    revoked_exit();
                }
                if since.elapsed() < budget {
                    std::thread::yield_now();
                    continue;
                }
                poll(*fd, libc::POLLIN);
                since = Instant::now();
            },
        }
    }
}

fn poll(fd: RawFd, events: i16) {
    let mut p = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut p, 1, -1) };
}

/// `pb8 app DESIGN caller|target ROUNDS CASTS BUDGET_NS`, with its channel
/// at descriptor 3 (and, for the hub, its eventfd at 4).
fn app(args: &[String]) {
    let design = Design::parse(&args[0]);
    let caller = args[1] == "caller";
    let rounds: u64 = args[2].parse().unwrap();
    let casts: u64 = args[3].parse().unwrap();
    let budget = Duration::from_nanos(args[4].parse().unwrap());
    let chan = match design {
        Design::Hub => {
            let base = map(3, 2 * RING);
            // SAFETY: two rings inside the mapping, which lives as long as
            // the process.
            let (rx, tx) = unsafe { (Ring::new(base), Ring::new(base.add(RING))) };
            Chan::Rings {
                rx,
                tx,
                wake: Wake::Bell(4),
                revoked: None,
            }
        }
        Design::Ring => {
            let base = map(3, PAIR_LEN);
            // SAFETY: as above; the revoked word is aligned inside it.
            let (c2t, t2c, revoked) = unsafe {
                (
                    Ring::new(base),
                    Ring::new(base.add(RING)),
                    &*(base.add(REVOKED_AT) as *const AtomicU32),
                )
            };
            let (rx, tx) = if caller { (t2c, c2t) } else { (c2t, t2c) };
            Chan::Rings {
                rx,
                tx,
                wake: Wake::Futex,
                revoked: Some(revoked),
            }
        }
        Design::Socket => Chan::Socket(3),
    };
    let mut buf = Vec::with_capacity(64);
    if !caller {
        let mut count = 0u64;
        loop {
            chan.recv(&mut buf, budget);
            match buf[0] {
                CALL => chan.send(&frame(REPLY, field(&buf, 1), count)),
                CAST if field(&buf, 1) == STOP => break,
                CAST => count += 1,
                _ => std::process::exit(6),
            }
        }
        println!("target {}", cpu_us(libc::RUSAGE_SELF));
        return;
    }
    let call = |seq: u64, buf: &mut Vec<u8>| {
        chan.send(&frame(CALL, seq, 0));
        chan.recv(buf, budget);
        assert_eq!((buf[0], field(buf, 1)), (REPLY, seq));
        field(buf, 9)
    };
    for seq in 0..WARM {
        call(seq, &mut buf);
    }
    let start = now_ns();
    let mut lat = Vec::new();
    if casts == 0 {
        // An endless run (`pb8 revoke`) keeps the first 4 Mi latencies.
        let keep = rounds.min(1 << 22) as usize;
        lat.reserve(keep);
        for seq in 0..rounds {
            let t = now_ns();
            call(WARM + seq, &mut buf);
            if lat.len() < keep {
                lat.push((now_ns() - t) as u32);
            }
        }
    } else {
        for seq in 0..casts {
            chan.send(&frame(CAST, seq, 0));
        }
        let counted = call(WARM, &mut buf);
        assert_eq!(counted, casts);
    }
    let end = now_ns();
    chan.send(&frame(CAST, STOP, 0));
    lat.sort_unstable();
    let pct = |p: f64| lat.get(((lat.len() as f64) * p) as usize).copied().unwrap_or(0);
    println!(
        "caller {start} {end} {} {} {}",
        pct(0.5),
        pct(0.99),
        cpu_us(libc::RUSAGE_SELF)
    );
}

// ---------------------------------------------------------------- hub side

/// One app as the hub sees it: its ring to the hub, the ring the hub
/// writes to it, and the eventfd it rings.
struct HubApp {
    rx: Ring,
    tx: Ring,
    bell: RawFd,
}

/// Forward frames between each app and its pair's other app until `stop`;
/// the thread's CPU in µs.
fn hub_thread(pairs: Vec<(HubApp, HubApp)>, budget: Duration, stop: Arc<AtomicBool>) -> u64 {
    // SAFETY: epoll_create1 and epoll_ctl on eventfds this thread owns.
    let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    for (a, b) in &pairs {
        for app in [a, b] {
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: app.bell as u64,
            };
            unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, app.bell, &mut ev) };
        }
    }
    let mut buf = Vec::with_capacity(64);
    let mut last = Instant::now();
    loop {
        let mut any = false;
        for (a, b) in &pairs {
            for (from, to) in [(a, b), (b, a)] {
                while from.rx.pop(&mut buf).unwrap() {
                    any = true;
                    if to.tx.push(&buf) {
                        futex_wake(to.tx.waiting());
                    }
                }
            }
        }
        if any {
            last = Instant::now();
            continue;
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if last.elapsed() < budget {
            std::thread::yield_now();
            continue;
        }
        let mut all = true;
        for (a, b) in &pairs {
            for app in [a, b] {
                all &= app.rx.sleep();
            }
        }
        if all {
            let mut events = [libc::epoll_event { events: 0, u64: 0 }; 64];
            // SAFETY: a valid buffer of 64 events.
            let n = unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), 64, 50) };
            for e in events.iter().take(n.max(0) as usize) {
                drain(e.u64 as RawFd);
            }
        }
        for (a, b) in &pairs {
            for app in [a, b] {
                app.rx.waiting().store(0, Ordering::SeqCst);
            }
        }
        last = Instant::now();
    }
    cpu_us(libc::RUSAGE_THREAD)
}

// ---------------------------------------------------------------- bench

/// One measurement's result.
#[derive(Clone, Copy, Default)]
struct Result {
    p50_ns: u32,
    p99_ns: u32,
    ops_per_s: f64,
    cpu_us_per_op: f64,
}

/// Start `pb8 app` with `fds` at descriptors 3, 4, ...
fn spawn(fds: &[RawFd], args: &[String]) -> Child {
    let fds = fds.to_vec();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.arg("app").args(args).stdout(Stdio::piped());
    // SAFETY: the closure makes system calls only.
    unsafe {
        cmd.pre_exec(move || {
            for (i, &fd) in fds.iter().enumerate() {
                let to = 3 + i as RawFd;
                if fd == to {
                    libc::fcntl(fd, libc::F_SETFD, 0);
                } else if libc::dup2(fd, to) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    cmd.spawn().unwrap()
}

/// The resources one pair holds on the bench's side, to revoke it.
enum Held {
    Hub,
    Ring(*mut u8),
    Socket(RawFd),
}

/// Start a pair; its children and what the bench keeps.
fn pair(
    design: Design,
    rounds: u64,
    casts: u64,
    budget: Duration,
    hub: &mut Vec<(HubApp, HubApp)>,
) -> (Child, Child, Held) {
    let args = |role: &str| {
        vec![
            design.name().to_string(),
            role.to_string(),
            rounds.to_string(),
            casts.to_string(),
            budget.as_nanos().to_string(),
        ]
    };
    match design {
        Design::Hub => {
            let make = |role: &str| {
                let fd = memfd(2 * RING);
                let bell = eventfd();
                let child = spawn(&[fd, bell], &args(role));
                let base = map(fd, 2 * RING);
                // SAFETY: the bench keeps the mapping for the process's life.
                // The app reads the ring at 0 and writes the one at RING.
                let app = unsafe {
                    HubApp {
                        tx: Ring::new(base),
                        rx: Ring::new(base.add(RING)),
                        bell,
                    }
                };
                unsafe { libc::close(fd) };
                (child, app)
            };
            let (target, t) = make("target");
            let (caller, c) = make("caller");
            hub.push((c, t));
            (caller, target, Held::Hub)
        }
        Design::Ring => {
            let fd = memfd(PAIR_LEN);
            let base = map(fd, PAIR_LEN);
            let target = spawn(&[fd], &args("target"));
            let caller = spawn(&[fd], &args("caller"));
            unsafe { libc::close(fd) };
            (caller, target, Held::Ring(base))
        }
        Design::Socket => {
            let mut sv = [0; 2];
            // SAFETY: a valid array of two descriptors.
            let r = unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    sv.as_mut_ptr(),
                )
            };
            assert_eq!(r, 0);
            let target = spawn(&[sv[0]], &args("target"));
            let caller = spawn(&[sv[1]], &args("caller"));
            // The bench keeps one end's descriptor to shut the pair down.
            unsafe { libc::close(sv[1]) };
            (caller, target, Held::Socket(sv[0]))
        }
    }
}

fn revoke(held: &Held) {
    match held {
        Held::Hub => {}
        Held::Ring(base) => {
            // SAFETY: the bench's own mapping of the pair.
            let (word, c2t, t2c) = unsafe {
                (
                    &*(base.add(REVOKED_AT) as *const AtomicU32),
                    Ring::new(*base),
                    Ring::new(base.add(RING)),
                )
            };
            word.store(1, Ordering::Release);
            for ring in [c2t, t2c] {
                ring.waiting().store(0, Ordering::SeqCst);
                futex_wake(ring.waiting());
            }
        }
        Held::Socket(fd) => {
            // SAFETY: the bench's own descriptor.
            unsafe { libc::shutdown(*fd, libc::SHUT_RDWR) };
        }
    }
}

fn release(held: Held) {
    match held {
        Held::Hub => {}
        Held::Ring(base) => unsafe {
            libc::munmap(base.cast(), PAIR_LEN);
        },
        Held::Socket(fd) => unsafe {
            libc::close(fd);
        },
    }
}

fn output(child: Child) -> String {
    let mut out = String::new();
    let mut child = child;
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    assert!(child.wait().unwrap().success(), "{out}");
    out
}

fn run(design: Design, pairs: usize, rounds: u64, casts: u64, budget: Duration) -> Result {
    let mut hub = Vec::new();
    let mut children = Vec::new();
    for _ in 0..pairs {
        let (caller, target, held) = pair(design, rounds, casts, budget, &mut hub);
        children.push((caller, target, held));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut groups: Vec<Vec<(HubApp, HubApp)>> = (0..HUB_THREADS).map(|_| Vec::new()).collect();
    for (i, p) in hub.into_iter().enumerate() {
        groups[i % HUB_THREADS].push(p);
    }
    let threads: Vec<_> = groups
        .into_iter()
        .filter(|g| !g.is_empty())
        .map(|g| {
            let stop = stop.clone();
            std::thread::spawn(move || hub_thread(g, budget, stop))
        })
        .collect();
    let (mut start, mut end, mut cpu) = (u64::MAX, 0u64, 0u64);
    let (mut p50s, mut p99s) = (Vec::new(), Vec::new());
    for (caller, target, held) in children {
        let c = output(caller);
        let t = output(target);
        let f: Vec<u64> = c.split_whitespace().skip(1).map(|v| v.parse().unwrap()).collect();
        start = start.min(f[0]);
        end = end.max(f[1]);
        p50s.push(f[2] as f64);
        p99s.push(f[3] as f64);
        cpu += f[4] + t.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap();
        release(held);
    }
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        cpu += t.join().unwrap();
    }
    let per = if casts == 0 { rounds } else { casts };
    let ops = (per * pairs as u64) as f64;
    // CPU over each process's whole life, per timed operation; the warm-up
    // and the start are included.
    let all_ops = ops + (WARM * pairs as u64) as f64;
    // Each caller's own percentiles; the median across callers.
    Result {
        p50_ns: median(p50s) as u32,
        p99_ns: median(p99s) as u32,
        ops_per_s: ops / ((end - start) as f64 / 1e9),
        cpu_us_per_op: cpu as f64 / all_ops,
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn bench(args: &[String]) {
    let rounds_of_runs: usize = args.first().and_then(|a| a.parse().ok()).unwrap_or(3);
    let budget = Duration::from_micros(20);
    // (label, pairs, calls per caller, casts per caller)
    let cases: [(&str, usize, u64, u64); 4] = [
        ("call, one pair", 1, 50_000, 0),
        ("call, 16 pairs", 16, 20_000, 0),
        ("cast, one pair", 1, 0, 400_000),
        ("cast, 16 pairs", 16, 0, 100_000),
    ];
    println!(
        "pb8: budget {} µs, {} hub threads, {} rounds, 16 B bodies",
        budget.as_micros(),
        HUB_THREADS,
        rounds_of_runs
    );
    for (label, pairs, calls, casts) in cases {
        let mut all: Vec<Vec<Result>> = vec![Vec::new(); 3];
        for _ in 0..rounds_of_runs {
            for (i, d) in Design::ALL.into_iter().enumerate() {
                all[i].push(run(d, pairs, calls, casts, budget));
            }
        }
        println!("\n{label}:");
        for (i, d) in Design::ALL.into_iter().enumerate() {
            let r = &all[i];
            let m = |f: fn(&Result) -> f64| median(r.iter().map(f).collect());
            let each = r
                .iter()
                .map(|x| format!("{:.0}", x.ops_per_s))
                .collect::<Vec<_>>()
                .join(" ");
            println!(
                "  {:7} p50 {:6.2} µs  p99 {:6.2} µs  {:>10.0} ops/s  {:6.2} µs CPU/op  (ops/s each: {each})",
                d.name(),
                m(|x| f64::from(x.p50_ns)) / 1000.0,
                m(|x| f64::from(x.p99_ns)) / 1000.0,
                m(|x| x.ops_per_s),
                m(|x| x.cpu_us_per_op),
            );
        }
    }
}

/// Revoke a busy pair of each direct design; how long until both stop.
fn revocation() {
    let budget = Duration::from_micros(20);
    for design in [Design::Ring, Design::Socket] {
        let mut hub = Vec::new();
        let (mut caller, mut target, held) = pair(design, u64::MAX / 2, 0, budget, &mut hub);
        std::thread::sleep(Duration::from_millis(300));
        let t = Instant::now();
        revoke(&held);
        let c = caller.wait().unwrap();
        let tg = target.wait().unwrap();
        let took = t.elapsed();
        println!(
            "{}: both stopped {:.1} µs after revoking; exit codes {:?} and {:?}",
            design.name(),
            took.as_secs_f64() * 1e6,
            c.code(),
            tg.code()
        );
        release(held);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("app") => app(&args[1..]),
        Some("bench") => bench(&args[1..]),
        Some("revoke") => revocation(),
        _ => eprintln!("usage: pb8 bench [ROUNDS] | pb8 revoke"),
    }
}
