//! The cost of epoch interruption on guest compute, for `docs/BASELINE.md`.
//!
//! Runs three WebAssembly kernels on two wasmtime engines that differ only in
//! `epoch_interruption`. The epoch engine is driven as `paddock-host` drives
//! it: a ticker thread advances the epoch every 10 ms, and each tick makes
//! the guest yield to tokio (`UpdateDeadline::Yield(1)`). Both engines call
//! through `call_async`, so the fiber cost is the same on both sides.
//!
//! ```sh
//! cargo run --release -p paddock-host --example epoch_cost
//! ```
//!
//! The kernels are hand-written WAT, chosen to stress what epoch checks
//! cost: a tight loop (a check on every back edge), deep recursion (a check
//! on every function entry) and a sieve over linear memory (nested loops
//! with loads and stores).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use wasmtime::{Config, Engine, Instance, Module, Store, UpdateDeadline, WasmParams, WasmResults};

const KERNELS: &str = r#"
(module
  (memory 256)
  ;; Linear congruential generator, $n rounds.
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
  ;; Naive recursive Fibonacci.
  (func $fib (export "fib") (param $n i32) (result i32)
    (if (result i32) (i32.lt_u (local.get $n) (i32.const 2))
      (then (local.get $n))
      (else
        (i32.add
          (call $fib (i32.sub (local.get $n) (i32.const 1)))
          (call $fib (i32.sub (local.get $n) (i32.const 2)))))))
  ;; Sieve of Eratosthenes below $n (at most 16 MiB); returns the prime count.
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

const SAMPLES: usize = 7;
const TICK: Duration = Duration::from_millis(10);

struct Side {
    engine: Engine,
    module: Module,
    epoch: bool,
}

impl Side {
    fn new(epoch: bool) -> Self {
        let mut config = Config::new();
        config.epoch_interruption(epoch);
        let engine = Engine::new(&config).unwrap();
        let module = Module::new(&engine, wat::parse_str(KERNELS).unwrap()).unwrap();
        Self {
            engine,
            module,
            epoch,
        }
    }

    async fn store(&self) -> (Store<()>, Instance) {
        let mut store = Store::new(&self.engine, ());
        if self.epoch {
            store.epoch_deadline_callback(|_| Ok(UpdateDeadline::Yield(1)));
            store.set_epoch_deadline(1);
        }
        let instance = Instance::new_async(&mut store, &self.module, &[])
            .await
            .unwrap();
        (store, instance)
    }

    /// One timed run of `name(arg)`, with its result.
    async fn run<P, R>(&self, name: &str, arg: P) -> (Duration, R)
    where
        P: WasmParams + Send + Sync,
        R: WasmResults + Send + Sync,
    {
        let (mut store, instance) = self.store().await;
        let func = instance.get_typed_func::<P, R>(&mut store, name).unwrap();
        let started = Instant::now();
        let result = func.call_async(&mut store, arg).await.unwrap();
        (started.elapsed(), result)
    }
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1e3)
}

async fn compare<P, R>(off: &Side, on: &Side, name: &str, label: &str, arg: P)
where
    P: WasmParams + Send + Sync + Copy,
    R: WasmResults + Send + Sync + PartialEq + std::fmt::Debug,
{
    let (mut without, mut with) = (Vec::new(), Vec::new());
    // Warm up both, then alternate so drift affects both sides alike.
    let (_, expected) = off.run::<P, R>(name, arg).await;
    let (_, got) = on.run::<P, R>(name, arg).await;
    assert_eq!(got, expected, "{name}: results differ");
    for _ in 0..SAMPLES {
        without.push(off.run::<P, R>(name, arg).await.0);
        with.push(on.run::<P, R>(name, arg).await.0);
    }
    let (a, b) = (median(without), median(with));
    println!(
        "{label}: epoch off {}, epoch on {}, ratio {:.3}",
        ms(a),
        ms(b),
        b.as_secs_f64() / a.as_secs_f64()
    );
}

fn main() {
    let off = Side::new(false);
    let on = Side::new(true);
    let stop = Arc::new(AtomicBool::new(false));
    let ticker = {
        let (engine, stop) = (on.engine.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(TICK);
                engine.increment_epoch();
            }
        })
    };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            println!(
                "median of {SAMPLES} runs; epoch tick {} ms, yield per tick",
                TICK.as_millis()
            );
            compare::<i64, i64>(&off, &on, "lcg", "tight loop, 500M rounds", 500_000_000).await;
            compare::<i32, i32>(&off, &on, "fib", "recursion, fib(35)", 35).await;
            compare::<i32, i32>(&off, &on, "sieve", "sieve, 16M", 16 << 20).await;
        });
    stop.store(true, Ordering::Relaxed);
    ticker.join().unwrap();
}
