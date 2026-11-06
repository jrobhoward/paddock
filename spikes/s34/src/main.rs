//! Spike S34: a waker for a native task's rings.
//!
//! A tokio task paired with an "app" thread stands for a native instance's
//! task and its app process: the task writes a request word, the app
//! thread (spinning with `sched_yield`, as apps do) sees it, works for
//! `HANDLE` and writes the reply word, and the task waits for the reply by
//! one of:
//! - `yield`: check, else `yield_now` (today's fallback);
//! - `watch`: check for up to 5 µs within the poll, then `yield_now`
//!   (T9's watch);
//! - `waker`: register the task's waker and return `Pending`; a watcher
//!   thread scans the waiting slots and wakes each task whose reply came;
//! - `hybrid`: `watch` while no more tasks wait than there are workers,
//!   `waker` beyond;
//! - `park`: `yield`, and when a yield came back with no other task run on
//!   the worker meanwhile, register the waker and return `Pending`: the
//!   worker, with nothing to run, parks, and its park hook
//!   (`on_thread_park`) watches the waiting slots for up to `PARK_WATCH`,
//!   waking the tasks whose replies came (or, at the end, every waiting
//!   task, to look again).
//!
//! Each case runs `pairs` pairs at once for `secs` seconds on `workers`
//! tokio workers, beside `work` tasks standing for the node's other work
//! (each a loop of a 1 µs burst and a `yield_now`): round trips per
//! second, the task's p50 and p99 round trip (from writing the request to
//! running again with the reply), the work tasks' bursts per second (what
//! a waiting task holding a worker costs the rest), and the process's CPU
//! (workers, app threads and the watcher) in cores.
//!
//! ```sh
//! cargo build --release -p s34
//! target/release/s34 [workers] [secs] [pairs,...] [methods,...] [work]
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use atomic_waker::AtomicWaker;

/// What an app does with a request before it replies.
const HANDLE: Duration = Duration::from_nanos(500);
/// T9's watch slice.
const WATCH: Duration = Duration::from_micros(5);
/// How long a parking worker's hook watches the waiting slots.
const PARK_WATCH: Duration = Duration::from_micros(20);

/// One unit of the node's other work.
const BURST: Duration = Duration::from_micros(1);

/// Round trips each task keeps for the percentiles.
const SAMPLES: usize = 200_000;

/// One pair's shared words.
struct Slot {
    req: AtomicU64,
    rep: AtomicU64,
    waiting: AtomicBool,
    waker: AtomicWaker,
}

impl Slot {
    fn new() -> Self {
        Self {
            req: AtomicU64::new(0),
            rep: AtomicU64::new(0),
            waiting: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Method {
    Yield,
    Watch,
    Waker,
    Hybrid,
    Park,
}

impl Method {
    fn parse(s: &str) -> Self {
        match s {
            "yield" => Self::Yield,
            "watch" => Self::Watch,
            "waker" => Self::Waker,
            "hybrid" => Self::Hybrid,
            "park" => Self::Park,
            other => panic!("unknown method {other}"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Yield => "yield",
            Self::Watch => "watch",
            Self::Waker => "waker",
            Self::Hybrid => "hybrid",
            Self::Park => "park",
        }
    }

    fn uses_watcher(self) -> bool {
        matches!(self, Self::Waker | Self::Hybrid)
    }
}

/// Tasks waiting for a reply now (the hybrid's count).
static WAITING: AtomicUsize = AtomicUsize::new(0);

/// The slots of the case running now, for the park hook.
static SLOTS: Mutex<Option<Arc<Vec<Arc<Slot>>>>> = Mutex::new(None);

thread_local! {
    /// How many times one of this case's tasks resumed on this thread.
    static TURN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn turn() -> u64 {
    TURN.with(std::cell::Cell::get)
}

fn bump() {
    TURN.with(|t| t.set(t.get() + 1));
}

/// The park hook: watch the waiting slots for up to `PARK_WATCH`, waking
/// each task whose reply came; at the end, wake every waiting task.
fn park_hook() {
    let Some(slots) = SLOTS.lock().unwrap().clone() else {
        return;
    };
    let until = Instant::now() + PARK_WATCH;
    loop {
        let mut any = false;
        let mut woke = false;
        for slot in slots.iter() {
            if slot.waiting.load(Ordering::Acquire) {
                any = true;
                if slot.rep.load(Ordering::Acquire) >= slot.req.load(Ordering::Relaxed)
                    && slot.waiting.swap(false, Ordering::AcqRel)
                {
                    slot.waker.wake();
                    woke = true;
                }
            }
        }
        if woke || !any {
            return;
        }
        if Instant::now() >= until {
            for slot in slots.iter() {
                if slot.waiting.swap(false, Ordering::AcqRel) {
                    slot.waker.wake();
                }
            }
            return;
        }
        std::hint::spin_loop();
    }
}

fn cpu_time() -> Duration {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `u` is a valid rusage for getrusage to fill.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(u.ru_utime) + tv(u.ru_stime)
}

fn app(slot: Arc<Slot>, stop: Arc<AtomicBool>) {
    let mut seen = 0;
    while !stop.load(Ordering::Relaxed) {
        let req = slot.req.load(Ordering::Acquire);
        if req == seen {
            std::thread::yield_now();
            continue;
        }
        seen = req;
        let t = Instant::now();
        while t.elapsed() < HANDLE {
            std::hint::spin_loop();
        }
        slot.rep.store(req, Ordering::Release);
    }
}

fn watcher(slots: Arc<Vec<Arc<Slot>>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let mut any = false;
        for slot in slots.iter() {
            if slot.waiting.load(Ordering::Acquire) {
                any = true;
                if slot.rep.load(Ordering::Acquire) >= slot.req.load(Ordering::Relaxed)
                    && slot.waiting.swap(false, Ordering::AcqRel)
                {
                    slot.waker.wake();
                }
            }
        }
        if any {
            std::hint::spin_loop();
        } else {
            std::thread::yield_now();
        }
    }
}

async fn wait_park(slot: &Slot, seq: u64) {
    loop {
        if slot.rep.load(Ordering::Acquire) >= seq {
            return;
        }
        let before = turn();
        tokio::task::yield_now().await;
        bump();
        if slot.rep.load(Ordering::Acquire) >= seq {
            return;
        }
        if turn() == before + 1 {
            // Nothing else ran here: let the worker park and watch.
            wait_waker(slot, seq).await;
            bump();
        }
    }
}

async fn wait_yield(slot: &Slot, seq: u64) {
    while slot.rep.load(Ordering::Acquire) < seq {
        tokio::task::yield_now().await;
    }
}

async fn wait_watch(slot: &Slot, seq: u64) {
    loop {
        let until = Instant::now() + WATCH;
        loop {
            for _ in 0..16 {
                if slot.rep.load(Ordering::Acquire) >= seq {
                    return;
                }
                std::hint::spin_loop();
            }
            if Instant::now() >= until {
                break;
            }
        }
        tokio::task::yield_now().await;
    }
}

async fn wait_waker(slot: &Slot, seq: u64) {
    std::future::poll_fn(|cx| {
        if slot.rep.load(Ordering::Acquire) >= seq {
            return Poll::Ready(());
        }
        slot.waker.register(cx.waker());
        slot.waiting.store(true, Ordering::SeqCst);
        if slot.rep.load(Ordering::Acquire) >= seq {
            slot.waiting.store(false, Ordering::SeqCst);
            return Poll::Ready(());
        }
        Poll::Pending
    })
    .await;
}

async fn pair_task(
    slot: Arc<Slot>,
    method: Method,
    workers: usize,
    until: Instant,
    out: Arc<Mutex<(u64, Vec<u64>)>>,
) {
    let mut seq = 0;
    let mut ns = Vec::with_capacity(SAMPLES);
    let mut n = 0u64;
    while Instant::now() < until {
        seq += 1;
        let t = Instant::now();
        slot.req.store(seq, Ordering::Release);
        let waiting = WAITING.fetch_add(1, Ordering::Relaxed) + 1;
        match method {
            Method::Yield => wait_yield(&slot, seq).await,
            Method::Watch => wait_watch(&slot, seq).await,
            Method::Waker => wait_waker(&slot, seq).await,
            Method::Hybrid if waiting <= workers => wait_watch(&slot, seq).await,
            Method::Hybrid => wait_waker(&slot, seq).await,
            Method::Park => wait_park(&slot, seq).await,
        }
        WAITING.fetch_sub(1, Ordering::Relaxed);
        if ns.len() < SAMPLES {
            ns.push(u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }
        n += 1;
    }
    let mut o = out.lock().unwrap();
    o.0 += n;
    o.1.extend(ns);
}

async fn work_task(until: Instant, done: Arc<AtomicU64>) {
    let mut n = 0;
    while Instant::now() < until {
        let t = Instant::now();
        while t.elapsed() < BURST {
            std::hint::spin_loop();
        }
        n += 1;
        tokio::task::yield_now().await;
        bump();
    }
    done.fetch_add(n, Ordering::Relaxed);
}

fn case(workers: usize, secs: f64, pairs: usize, method: Method, work: usize) {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(workers).enable_all();
    // `S34_GQI`: how many ticks a worker runs between looks at the global
    // queue, where a task woken from outside the runtime waits (tokio: 61).
    if let Some(gqi) = std::env::var("S34_GQI").ok().and_then(|v| v.parse().ok()) {
        builder.global_queue_interval(gqi);
    }
    if method == Method::Park {
        builder.on_thread_park(park_hook);
    }
    let rt = builder.build().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let slots: Arc<Vec<Arc<Slot>>> = Arc::new((0..pairs).map(|_| Arc::new(Slot::new())).collect());
    let mut threads = Vec::new();
    for slot in slots.iter() {
        let (slot, stop) = (slot.clone(), stop.clone());
        threads.push(std::thread::spawn(move || app(slot, stop)));
    }
    *SLOTS.lock().unwrap() = Some(slots.clone());
    if method.uses_watcher() {
        let (slots, stop) = (slots.clone(), stop.clone());
        threads.push(std::thread::spawn(move || watcher(slots, stop)));
    }
    std::thread::sleep(Duration::from_millis(50));
    let out = Arc::new(Mutex::new((0u64, Vec::new())));
    let cpu0 = cpu_time();
    let t0 = Instant::now();
    let until = t0 + Duration::from_secs_f64(secs);
    let bursts = Arc::new(AtomicU64::new(0));
    rt.block_on(async {
        let mut tasks: Vec<_> = slots
            .iter()
            .map(|slot| tokio::spawn(pair_task(slot.clone(), method, workers, until, out.clone())))
            .collect();
        for _ in 0..work {
            tasks.push(tokio::spawn(work_task(until, bursts.clone())));
        }
        for t in tasks {
            t.await.unwrap();
        }
    });
    let wall = t0.elapsed();
    let cpu = cpu_time() - cpu0;
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().unwrap();
    }
    let (n, mut ns) = {
        let o = out.lock().unwrap();
        (o.0, o.1.clone())
    };
    ns.sort_unstable();
    let at = |p: f64| ns.get(((ns.len().saturating_sub(1)) as f64 * p) as usize).copied().unwrap_or(0) as f64 / 1000.0;
    println!(
        "{:<6} {:>3} pairs, {:>2} work: {:>9.0} round trips/s, p50 {:>6.2} µs, p99 {:>7.2} µs, work {:>9.0}/s, {:>5.2} cores",
        method.name(),
        pairs,
        work,
        n as f64 / wall.as_secs_f64(),
        at(0.5),
        at(0.99),
        bursts.load(Ordering::Relaxed) as f64 / wall.as_secs_f64(),
        cpu.as_secs_f64() / wall.as_secs_f64()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let workers: usize = args.first().map_or(4, |a| a.parse().unwrap());
    let secs: f64 = args.get(1).map_or(2.0, |a| a.parse().unwrap());
    let pairs: Vec<usize> = args
        .get(2)
        .map_or_else(|| vec![1, 4, 16, 64], |a| a.split(',').map(|p| p.parse().unwrap()).collect());
    let methods: Vec<Method> = args
        .get(3)
        .map_or_else(
            || vec![Method::Yield, Method::Watch, Method::Waker, Method::Hybrid, Method::Park],
            |a| a.split(',').map(Method::parse).collect(),
        );
    let work: usize = args.get(4).map_or(0, |a| a.parse().unwrap());
    println!(
        "s34: {workers} workers, {secs} s a case, {} cpus, app handling {} ns, {work} work tasks",
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        HANDLE.as_nanos()
    );
    for &p in &pairs {
        for &m in &methods {
            case(workers, secs, p, m, work);
        }
    }
}
