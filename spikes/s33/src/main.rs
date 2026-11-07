//! Spike S33: waiting on a cache line instead of spinning.
//!
//! Pairs of threads ping-pong over words laid out as a ring's are (the
//! position written by one side and the other side's "I sleep" flag on
//! lines of their own). Each side waits for the other's word to change by
//! spinning up to `budget` with one method, then sleeping on a futex
//! after setting its flag, as `bulkhead-native`'s channel does:
//! - `yield`: `sched_yield` each turn (today's channel);
//! - `pause`: a spin-loop hint each turn (S32's pause);
//! - `line`: wait in the CPU for the word's line to be written
//!   (`MWAITX` on AMD x86-64, a load-exclusive and `WFE` on arm64), up to
//!   the budget's end;
//! - `lineyield`: wait on the line for up to `slice`, then `sched_yield`,
//!   and again: a waiter that wakes within nanoseconds of a write, but
//!   gives its core up each slice.
//!
//! Each case runs `pairs` pairs for `secs` seconds beside `hogs` threads
//! standing for other work (each a loop counting): round trips per second,
//! the median of the pairs' p50 and the worst p99 (µs), CPU per round trip
//! (the process's user and system time, hogs excluded by subtracting their
//! own thread CPU), context switches per round trip, and the hogs' counts
//! per second against the same hogs alone.
//!
//! ```sh
//! cargo build --release -p s33
//! target/release/s33 [secs] [budget_us] [slice_us] [pairs,...] [methods,...] [hogs]
//! target/release/s33 idle     # how long one line wait lasts with no write
//! ```

mod wait;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

/// A word on a line (and a line pair) of its own.
#[repr(C, align(128))]
struct Padded<T>(T);

/// One direction: the writer's position and the reader's sleep flag.
struct Dir {
    word: Padded<AtomicU32>,
    waiting: Padded<AtomicU32>,
}

impl Dir {
    fn new() -> Self {
        Self {
            word: Padded(AtomicU32::new(0)),
            waiting: Padded(AtomicU32::new(0)),
        }
    }

    fn write(&self, v: u32) {
        self.word.0.store(v, Ordering::Release);
        fence(Ordering::SeqCst);
        let w = &self.waiting.0;
        if w.load(Ordering::SeqCst) == 1 && w.swap(0, Ordering::SeqCst) == 1 {
            futex_wake(w);
        }
    }
}

struct Pair {
    ping: Dir,
    pong: Dir,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Method {
    Yield,
    Pause,
    Line,
    LineYield,
}

impl Method {
    fn parse(s: &str) -> Self {
        match s {
            "yield" => Self::Yield,
            "pause" => Self::Pause,
            "line" => Self::Line,
            "lineyield" => Self::LineYield,
            _ => panic!("method: yield, pause, line or lineyield"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Yield => "yield",
            Self::Pause => "pause",
            Self::Line => "line",
            Self::LineYield => "lineyield",
        }
    }
}

#[derive(Clone, Copy)]
struct Wait {
    method: Method,
    budget: Duration,
    slice: Duration,
}

/// Counts of how waits ended, per thread.
#[derive(Default, Clone, Copy)]
struct Ends {
    spun: u64,
    slept: u64,
}

impl Wait {
    /// Wait until `dir`'s word is no longer `old`; its new value.
    fn until(&self, dir: &Dir, old: u32, ends: &mut Ends) -> u32 {
        let word = &dir.word.0;
        let v = word.load(Ordering::Acquire);
        if v != old {
            ends.spun += 1;
            return v;
        }
        if !self.budget.is_zero() {
            let start = Instant::now();
            let mut turn = 0u32;
            loop {
                match self.method {
                    Method::Yield => std::thread::yield_now(),
                    Method::Pause => std::hint::spin_loop(),
                    Method::Line => {
                        let left = self.budget.saturating_sub(start.elapsed());
                        wait::until_changed(word, old, left);
                    }
                    Method::LineYield => {
                        wait::until_changed(word, old, self.slice);
                        let v = word.load(Ordering::Acquire);
                        if v != old {
                            ends.spun += 1;
                            return v;
                        }
                        std::thread::yield_now();
                    }
                }
                let v = word.load(Ordering::Acquire);
                if v != old {
                    ends.spun += 1;
                    return v;
                }
                turn = turn.wrapping_add(1);
                let every = if self.method == Method::Pause { 64 } else { 1 };
                if turn.is_multiple_of(every) && start.elapsed() >= self.budget {
                    break;
                }
            }
        }
        ends.slept += 1;
        loop {
            dir.waiting.0.store(1, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            let v = word.load(Ordering::Acquire);
            if v != old {
                dir.waiting.0.store(0, Ordering::SeqCst);
                return v;
            }
            // On the flag, not the word, as the channel does: a writer late
            // from the round before may clear the flag just before this
            // sleeps, and then the wait must return at once (waiting on
            // the word, that lost the next wake and hung a pair).
            futex_wait(&dir.waiting.0, 1);
            let v = word.load(Ordering::Acquire);
            if v != old {
                dir.waiting.0.store(0, Ordering::SeqCst);
                return v;
            }
        }
    }
}

fn futex_wait(word: &AtomicU32, old: u32) {
    // SAFETY: FUTEX_WAIT on a valid, aligned u32 that outlives the call;
    // the kernel only reads it.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            old,
            std::ptr::null::<libc::timespec>(),
        );
    }
}

fn futex_wake(word: &AtomicU32) {
    // SAFETY: FUTEX_WAKE on a valid, aligned u32; it reads no memory.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            1,
        );
    }
}

/// The process's user + system CPU and context switches.
fn usage(who: i32) -> (Duration, u64, u64) {
    // SAFETY: getrusage writes into the zeroed struct it is given.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::getrusage(who, &raw mut u) };
    let t = |tv: libc::timeval| {
        Duration::from_secs(tv.tv_sec as u64) + Duration::from_micros(tv.tv_usec as u64)
    };
    (
        t(u.ru_utime) + t(u.ru_stime),
        u.ru_nvcsw as u64,
        u.ru_nivcsw as u64,
    )
}

/// Run `hogs` counting threads until `stop`; their counts per second and
/// their own CPU.
fn spawn_hogs(
    hogs: usize,
    stop: &Arc<AtomicBool>,
    start: &Arc<Barrier>,
) -> Vec<std::thread::JoinHandle<(u64, Duration)>> {
    (0..hogs)
        .map(|_| {
            let stop = stop.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..1000 {
                        n = std::hint::black_box(n + 1);
                    }
                }
                (n, usage(libc::RUSAGE_THREAD).0)
            })
        })
        .collect()
}

struct Result_ {
    rounds: u64,
    p50s: Vec<f64>,
    p99s: Vec<f64>,
    ends: Ends,
    secs: f64,
    cpu: Duration,
    vcsw: u64,
    ivcsw: u64,
    hog_count: u64,
}

fn run(secs: f64, pairs: usize, hogs: usize, w: Wait) -> Result_ {
    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2 * pairs + hogs + 1));
    let total = Arc::new(AtomicU64::new(0));
    let hog_threads = spawn_hogs(hogs, &stop, &barrier);
    let mut threads = Vec::new();
    for _ in 0..pairs {
        let pair = Arc::new(Pair {
            ping: Dir::new(),
            pong: Dir::new(),
        });
        // Server: echo each ping as a pong, until a ping of u32::MAX.
        {
            let pair = pair.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let mut ends = Ends::default();
                let mut last = 0;
                loop {
                    last = w.until(&pair.ping, last, &mut ends);
                    if last == u32::MAX {
                        break;
                    }
                    pair.pong.write(last);
                }
                (Vec::new(), ends)
            }));
        }
        // Client: ping, wait for the pong, time the round trip.
        {
            let pair = pair.clone();
            let barrier = barrier.clone();
            let stop = stop.clone();
            let total = total.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let mut ends = Ends::default();
                let mut samples = Vec::with_capacity(1 << 20);
                let mut i = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    i += 1;
                    let t = Instant::now();
                    pair.ping.write(i);
                    let mut got = i - 1;
                    while got != i {
                        got = w.until(&pair.pong, got, &mut ends);
                    }
                    if samples.len() < samples.capacity() {
                        samples.push(t.elapsed().as_nanos() as u32);
                    }
                }
                pair.ping.write(u32::MAX);
                total.fetch_add(u64::from(i), Ordering::Relaxed);
                (samples, ends)
            }));
        }
    }
    let (cpu0, v0, i0) = usage(libc::RUSAGE_SELF);
    barrier.wait();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(secs));
    stop.store(true, Ordering::Relaxed);
    let mut p50s = Vec::new();
    let mut p99s = Vec::new();
    let mut ends = Ends::default();
    for t in threads {
        let (mut s, e) = t.join().unwrap();
        ends.spun += e.spun;
        ends.slept += e.slept;
        if !s.is_empty() {
            s.sort_unstable();
            p50s.push(f64::from(s[s.len() / 2]) / 1000.0);
            p99s.push(f64::from(s[s.len() * 99 / 100]) / 1000.0);
        }
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let mut hog_count = 0;
    let mut hog_cpu = Duration::ZERO;
    for h in hog_threads {
        let (n, c) = h.join().unwrap();
        hog_count += n;
        hog_cpu += c;
    }
    let (cpu1, v1, i1) = usage(libc::RUSAGE_SELF);
    p50s.sort_by(f64::total_cmp);
    Result_ {
        rounds: total.load(Ordering::Relaxed),
        p50s,
        p99s,
        ends,
        secs: elapsed,
        cpu: (cpu1 - cpu0).saturating_sub(hog_cpu),
        vcsw: v1 - v0,
        ivcsw: i1 - i0,
        hog_count,
    }
}

fn list<T>(s: Option<&String>, default: &str, f: impl Fn(&str) -> T) -> Vec<T> {
    s.map_or(default, String::as_str).split(',').map(f).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "idle") {
        println!("line waits supported: {}", wait::supported());
        for timeout in [1u64, 5, 20, 100, 1000] {
            let mut d: Vec<_> = (0..200)
                .map(|_| wait::idle_wait(Duration::from_micros(timeout)).as_nanos() as u64)
                .collect();
            d.sort_unstable();
            println!(
                "one line wait, nothing written, timeout {timeout} us: p50 {:.1} us, p99 {:.1} us, max {:.1} us",
                d[d.len() / 2] as f64 / 1000.0,
                d[d.len() * 99 / 100] as f64 / 1000.0,
                d[d.len() - 1] as f64 / 1000.0
            );
        }
        return;
    }
    let secs: f64 = args.first().map_or(2.0, |s| s.parse().unwrap());
    let budget = Duration::from_micros(args.get(1).map_or(20, |s| s.parse().unwrap()));
    let slice = Duration::from_micros(args.get(2).map_or(2, |s| s.parse().unwrap()));
    let pairs = list(args.get(3), "1,8,32", |s| s.parse::<usize>().unwrap());
    let methods = list(args.get(4), "yield,pause,line,lineyield", Method::parse);
    let hogs: usize = args.get(5).map_or(0, |s| s.parse().unwrap());
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    assert!(
        wait::supported() || !methods.iter().any(|m| matches!(m, Method::Line | Method::LineYield)),
        "this CPU cannot wait on a cache line from user space"
    );
    println!(
        "s33: {cores} cores, {secs} s per case, budget {budget:?}, slice {slice:?}, hogs {hogs}"
    );
    let hogs_alone = if hogs > 0 {
        let r = run(secs, 0, hogs, Wait { method: Method::Yield, budget, slice });
        let per = r.hog_count as f64 / r.secs;
        println!("hogs alone: {:.0}M counts/s", per / 1e6);
        Some(per)
    } else {
        None
    };
    println!(
        "{:>5} {:>9} {:>12} {:>8} {:>9} {:>11} {:>8} {:>8} {:>7} {:>7}",
        "pairs", "method", "rounds/s", "p50 us", "p99 max", "CPU us/rt", "vcsw/rt", "ivcsw/rt", "slept%", "hogs%"
    );
    for &p in &pairs {
        for &m in &methods {
            let r = run(secs, p, hogs, Wait { method: m, budget, slice });
            let rt = r.rounds.max(1) as f64;
            let waits = (r.ends.spun + r.ends.slept).max(1) as f64;
            let hogs_pct = hogs_alone.map_or(String::from("-"), |alone| {
                format!("{:.0}", 100.0 * (r.hog_count as f64 / r.secs) / alone)
            });
            println!(
                "{:>5} {:>9} {:>12.0} {:>8.2} {:>9.1} {:>11.2} {:>8.3} {:>8.3} {:>7.1} {:>7}",
                p,
                m.name(),
                r.rounds as f64 / r.secs,
                r.p50s[r.p50s.len() / 2],
                r.p99s.iter().copied().fold(0.0, f64::max),
                r.cpu.as_secs_f64() * 1e6 / rt,
                r.vcsw as f64 / rt,
                r.ivcsw as f64 / rt,
                100.0 * r.ends.slept as f64 / waits,
                hogs_pct,
            );
        }
    }
}
