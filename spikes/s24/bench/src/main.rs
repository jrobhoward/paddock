//! Spike S24: MMU-based interruption (wasmtime PR #12990) against epoch
//! checks, on `epoch_cost`'s WAT kernels and `app_cost`'s core module.
//!
//! Five engines that differ only in how a running guest is interrupted:
//!
//! - `off`: no checks compiled in;
//! - `epoch idle`: epoch checks, the epoch never advanced (the checks alone);
//! - `epoch`: epoch checks, a tick every 2 ms and a yield on each, as
//!   `paddock-host` drives them (`UpdateDeadline::Yield(1)`);
//! - `mmu idle`: MMU checks, the timing wheel never ticked (the loads alone);
//! - `mmu`: MMU checks, `TimingWheelInterrupter::new(0)` ticked every 2 ms,
//!   so a running store is interrupted at the next tick, as with epochs.
//!
//! Every side runs through `call_async` on a one-worker tokio runtime; a
//! yield shows as a `Pending` poll of the call, counted per sample. Samples
//! alternate between the sides; tables have medians.
//!
//! ```sh
//! # in the main workspace, once:
//! cargo rustc --release -p paddock-workload --features exports \
//!     --crate-type cdylib --target wasm32-unknown-unknown --target-dir target/fixtures
//! # here:
//! cargo run --release [-- kernels|app]
//! ```

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant};

use paddock_workload::{Inputs, WORKLOADS};
use wasmtime::{
    Config, Engine, Instance, Module, Store, TimingWheelInterrupter, TypedFunc, UpdateDeadline,
    WasmParams, WasmResults,
};

const TICK: Duration = Duration::from_millis(2);
const SAMPLES: usize = 7;
const TARGET: Duration = Duration::from_millis(50);

const CORE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../target/fixtures/wasm32-unknown-unknown/release/paddock_workload.wasm"
);

const KERNELS: &str = r#"
(module
  (memory 256)
  (func (export "lcg") (param $n i64) (result i64)
    (local $x i64)
    (local.set $x (i64.const 1))
    (block $done
      (loop $next
        (br_if $done (i64.eqz (local.get $n)))
        (local.set $x
          (i64.add
            (i64.mul (local.get $x) (i64.const 6364136223846793005))
            (i64.const 1442695040888963407)))
        (local.set $n (i64.sub (local.get $n) (i64.const 1)))
        (br $next)))
    (local.get $x))
  (func $fib (export "fib") (param $n i32) (result i32)
    (if (result i32) (i32.lt_u (local.get $n) (i32.const 2))
      (then (local.get $n))
      (else
        (i32.add
          (call $fib (i32.sub (local.get $n) (i32.const 1)))
          (call $fib (i32.sub (local.get $n) (i32.const 2)))))))
  (func (export "sieve") (param $n i32) (result i32)
    (local $i i32) (local $j i32) (local $count i32)
    (memory.fill (i32.const 0) (i32.const 0) (local.get $n))
    (local.set $i (i32.const 2))
    (block $done
      (loop $outer
        (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
        (if (i32.eqz (i32.load8_u (local.get $i)))
          (then
            (local.set $count (i32.add (local.get $count) (i32.const 1)))
            (local.set $j (i32.add (local.get $i) (local.get $i)))
            (block $marked
              (loop $mark
                (br_if $marked (i32.ge_u (local.get $j) (local.get $n)))
                (i32.store8 (local.get $j) (i32.const 1))
                (local.set $j (i32.add (local.get $j) (local.get $i)))
                (br $mark)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (local.get $count))
)
"#;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Off,
    EpochIdle,
    Epoch,
    MmuIdle,
    Mmu,
}

const KINDS: [Kind; 5] = [Kind::Off, Kind::EpochIdle, Kind::Epoch, Kind::MmuIdle, Kind::Mmu];

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Off => "off",
            Kind::EpochIdle => "epoch idle",
            Kind::Epoch => "epoch",
            Kind::MmuIdle => "mmu idle",
            Kind::Mmu => "mmu",
        }
    }
}

/// One engine and what drives it.
struct Side {
    kind: Kind,
    engine: Engine,
    wheel: Option<Arc<TimingWheelInterrupter>>,
}

impl Side {
    fn new(kind: Kind) -> Self {
        let mut config = Config::new();
        let mut wheel = None;
        match kind {
            Kind::Off => {}
            Kind::EpochIdle | Kind::Epoch => {
                config.epoch_interruption(true);
            }
            Kind::MmuIdle | Kind::Mmu => {
                let w = Arc::new(TimingWheelInterrupter::new(0));
                config.mmu_interruption(true);
                config.with_mmu_interrupter(w.clone());
                wheel = Some(w);
            }
        }
        Self {
            kind,
            engine: Engine::new(&config).unwrap(),
            wheel,
        }
    }

    fn store(&self) -> Store<()> {
        let mut store = Store::new(&self.engine, ());
        if matches!(self.kind, Kind::EpochIdle | Kind::Epoch) {
            store.epoch_deadline_callback(|_| Ok(UpdateDeadline::Yield(1)));
            store.set_epoch_deadline(1);
        }
        store
    }

    /// Drive the ticked sides from one OS thread, as `paddock-host`'s
    /// epoch ticker does.
    fn ticker(sides: &[Side], stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        let epoch: Vec<Engine> = sides
            .iter()
            .filter(|s| s.kind == Kind::Epoch)
            .map(|s| s.engine.clone())
            .collect();
        let wheels: Vec<Arc<TimingWheelInterrupter>> = sides
            .iter()
            .filter(|s| s.kind == Kind::Mmu)
            .filter_map(|s| s.wheel.clone())
            .collect();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(TICK);
                epoch.iter().for_each(Engine::increment_epoch);
                wheels.iter().for_each(|w| w.tick());
            }
        })
    }
}

/// Await `f`, counting the polls that returned `Pending` (the yields).
async fn counted<F: Future>(f: F) -> (F::Output, u32) {
    let mut f = pin!(f);
    let mut pending = 0;
    let out = std::future::poll_fn(|cx| match f.as_mut().poll(cx) {
        Poll::Ready(v) => Poll::Ready(v),
        Poll::Pending => {
            pending += 1;
            Poll::Pending
        }
    })
    .await;
    (out, pending)
}

/// An instance of a module on one side, kept across samples.
struct Loaded {
    store: Store<()>,
    instance: Instance,
}

impl Loaded {
    async fn new(side: &Side, module: &Module) -> Self {
        let mut store = side.store();
        let instance = Instance::new_async(&mut store, module, &[]).await.unwrap();
        Self { store, instance }
    }

    fn func<P: WasmParams, R: WasmResults>(&mut self, name: &str) -> TypedFunc<P, R> {
        self.instance.get_typed_func(&mut self.store, name).unwrap()
    }

    async fn time<P, R>(&mut self, name: &str, arg: P) -> (Duration, R, u32)
    where
        P: WasmParams + Send + Sync,
        R: WasmResults + Send + Sync,
    {
        let f = self.func::<P, R>(name);
        let started = Instant::now();
        let (out, yields) = counted(f.call_async(&mut self.store, arg)).await;
        (started.elapsed(), out.unwrap(), yields)
    }
}

fn median<T: Ord + Copy>(mut v: Vec<T>) -> T {
    v.sort();
    v[v.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.2}", d.as_secs_f64() * 1e3)
}

fn ratio(a: Duration, b: Duration) -> String {
    format!("{:.3}", a.as_secs_f64() / b.as_secs_f64())
}

fn header(first: &str) {
    println!(
        "| {first} | off ms | epoch idle | epoch | mmu idle | mmu | epoch idle/off | epoch/off | mmu idle/off | mmu/off | yields epoch, mmu |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
}

fn row(label: &str, times: &[Duration], yields: &[u32]) {
    let off = times[0];
    let t: Vec<String> = times[1..].iter().map(|&d| ms(d)).collect();
    let r: Vec<String> = times[1..].iter().map(|&d| ratio(d, off)).collect();
    println!(
        "| {label} | {} | {} | {} | {}, {} |",
        ms(off),
        t.join(" | "),
        r.join(" | "),
        yields[2],
        yields[4]
    );
}

/// Sample every side `SAMPLES` times, alternating; check results agree.
async fn sample<P, R>(loaded: &mut [Loaded], name: &str, arg: P) -> (Vec<Duration>, Vec<u32>)
where
    P: WasmParams + Send + Sync + Copy,
    R: WasmResults + Send + Sync + PartialEq + std::fmt::Debug,
{
    let mut expected = None;
    for l in loaded.iter_mut() {
        let (_, r, _) = l.time::<P, R>(name, arg).await;
        match &expected {
            None => expected = Some(r),
            Some(e) => assert_eq!(&r, e, "{name}: sides differ"),
        }
    }
    let mut times = vec![Vec::new(); loaded.len()];
    let mut yields = vec![Vec::new(); loaded.len()];
    for _ in 0..SAMPLES {
        for (i, l) in loaded.iter_mut().enumerate() {
            let (d, _, y) = l.time::<P, R>(name, arg).await;
            times[i].push(d);
            yields[i].push(y);
        }
    }
    (
        times.into_iter().map(median).collect(),
        yields.into_iter().map(median).collect(),
    )
}

async fn kernels(sides: &[Side]) {
    let wasm = KERNELS.as_bytes();
    let mut loaded = Vec::new();
    for s in sides {
        let module = Module::new(&s.engine, &wasm).unwrap();
        loaded.push(Loaded::new(s, &module).await);
    }
    println!("\nWAT kernels, median of {SAMPLES}, times in ms\n");
    header("Kernel");
    let (t, y) = sample::<i64, i64>(&mut loaded, "lcg", 500_000_000).await;
    row("lcg 500M", &t, &y);
    let (t, y) = sample::<i32, i32>(&mut loaded, "fib", 35).await;
    row("fib(35)", &t, &y);
    let (t, y) = sample::<i32, i32>(&mut loaded, "sieve", 16 << 20).await;
    row("sieve 16M", &t, &y);
}

async fn app(sides: &[Side]) {
    let wasm = std::fs::read(CORE).unwrap_or_else(|e| panic!("{CORE}: {e}"));
    println!("\nCompiled size of the core module\n");
    println!("| Side | Bytes |");
    println!("| --- | --- |");
    for s in sides.iter().filter(|s| matches!(s.kind, Kind::Off | Kind::Epoch | Kind::Mmu)) {
        let bytes = s.engine.precompile_module(&wasm).unwrap().len();
        println!("| {} | {bytes} |", s.kind.label());
    }
    let mut loaded = Vec::new();
    for s in sides {
        let module = Module::new(&s.engine, &wasm).unwrap();
        let mut l = Loaded::new(s, &module).await;
        let prepare = l.func::<(), ()>("workload_prepare");
        prepare.call_async(&mut l.store, ()).await.unwrap();
        loaded.push(l);
    }
    let inputs = Inputs::new();
    println!("\npaddock-workload core module, median of {SAMPLES}, times in ms\n");
    header("Workload");
    for (id, name) in (0u32..).zip(WORKLOADS) {
        let once = {
            let started = Instant::now();
            std::hint::black_box(paddock_workload::run(&inputs, name, 1));
            started.elapsed()
        };
        let n = u32::try_from((TARGET.as_nanos() / once.as_nanos().max(1)).max(1)).unwrap();
        let expected = paddock_workload::run(&inputs, name, n).unwrap();
        let (t, y) = sample::<(u32, u32), u64>(&mut loaded, "workload_run", (id, n)).await;
        let (_, got, _) = loaded[0].time::<(u32, u32), u64>("workload_run", (id, n)).await;
        assert_eq!(got, expected, "{name}: differs from native");
        row(&format!("{name} ×{n}"), &t, &y);
    }
}

fn main() {
    let what = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let sides: Vec<Side> = KINDS.iter().map(|&k| Side::new(k)).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let ticker = Side::ticker(&sides, stop.clone());
    println!(
        "S24: tick {} ms; wasmtime from PR #12990 (933d170); {}",
        TICK.as_millis(),
        std::env::consts::ARCH
    );
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            if what == "all" || what == "kernels" {
                kernels(&sides).await;
            }
            if what == "all" || what == "app" {
                app(&sides).await;
            }
        });
    stop.store(true, Ordering::Relaxed);
    ticker.join().unwrap();
}
