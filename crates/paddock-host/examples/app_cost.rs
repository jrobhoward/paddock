//! Ordinary app code in Wasm against the same code native, for
//! `docs/BASELINE.md` (ROADMAP B11): JSON with serde, a regex scan,
//! Markdown rendering and a `BTreeMap`, from `paddock-workload`.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-workload \
//!     --target-dir target/fixtures
//! cargo rustc --release -p paddock-workload --features exports \
//!     --crate-type cdylib --target wasm32-unknown-unknown --target-dir target/fixtures
//! cargo run --release -p paddock-host --example app_cost
//! ```
//!
//! Each workload runs the same number of times on four sides, the count
//! chosen so the native run takes about 50 ms:
//!
//! - native: `paddock_workload::run` in this process;
//! - Paddock: one call to `paddock-fixture-workload`, an app on a
//!   `Runtime` as configured by default (one round trip per sample);
//! - core module, epoch off and on: the same crate built as a core module
//!   with no imports, on two wasmtime engines that differ only in
//!   `epoch_interruption`, the epoch side driven as `paddock-host` drives
//!   it (a tick every `RuntimeConfig::epoch_tick`, a yield on each), as in
//!   `epoch_cost`.
//!
//! Every side's checksum must match the native one. Samples alternate
//! between the sides so drift affects them alike; the table has medians.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use paddock_host::{Envelope, InstanceHandle, InstanceSpec, Runtime, RuntimeConfig};
use paddock_workload::{Inputs, WORKLOADS};
use wasmtime::{Config, Engine, Instance, Module, Store, TypedFunc, UpdateDeadline};

const SAMPLES: usize = 7;
const TARGET: Duration = Duration::from_millis(50);
const LONG: Duration = Duration::from_secs(60);

const GUEST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/fixtures/wasm32-wasip2/release/paddock_fixture_workload.wasm"
);
const CORE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/fixtures/wasm32-unknown-unknown/release/paddock_workload.wasm"
);

/// The core module on one engine, instantiated once, inputs prepared.
struct Core {
    store: Store<()>,
    run: TypedFunc<(u32, u32), u64>,
}

impl Core {
    async fn new(engine: &Engine, wasm: &[u8], epoch: bool) -> Self {
        let module = Module::new(engine, wasm).unwrap();
        let mut store = Store::new(engine, ());
        if epoch {
            store.epoch_deadline_callback(|_| Ok(UpdateDeadline::Yield(1)));
            store.set_epoch_deadline(1);
        }
        let instance = Instance::new_async(&mut store, &module, &[])
            .await
            .expect("the core module has no imports");
        let prepare = instance
            .get_typed_func::<(), ()>(&mut store, "workload_prepare")
            .unwrap();
        prepare.call_async(&mut store, ()).await.unwrap();
        let run = instance
            .get_typed_func::<(u32, u32), u64>(&mut store, "workload_run")
            .unwrap();
        Self { store, run }
    }

    async fn time(&mut self, id: u32, n: u32) -> (Duration, u64) {
        let started = Instant::now();
        let sum = self.run.call_async(&mut self.store, (id, n)).await.unwrap();
        (started.elapsed(), sum)
    }
}

fn engine(epoch: bool) -> Engine {
    let mut config = Config::new();
    config.epoch_interruption(epoch);
    Engine::new(&config).unwrap()
}

fn native(inputs: &Inputs, name: &str, n: u32) -> (Duration, u64) {
    let started = Instant::now();
    let sum = paddock_workload::run(inputs, name, n).unwrap();
    (started.elapsed(), sum)
}

async fn guest(h: &InstanceHandle, name: &str, n: u32) -> (Duration, u64) {
    let started = Instant::now();
    let reply = h
        .call(Envelope::new("text/plain", format!("run:{name}:{n}")), LONG)
        .await
        .unwrap();
    let elapsed = started.elapsed();
    let text = String::from_utf8(reply.body).unwrap();
    (elapsed, text.parse().unwrap_or_else(|_| panic!("{text}")))
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1e3)
}

fn ratio(a: Duration, b: Duration) -> f64 {
    a.as_secs_f64() / b.as_secs_f64()
}

async fn run(component: Vec<u8>, core: Vec<u8>, on: Engine) {
    let inputs = Inputs::new();
    let (orders, log, markdown) = inputs.sizes();
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    let app = rt.load(&component).unwrap();
    let h = rt
        .spawn(
            &app,
            InstanceSpec {
                memory_max: 256 << 20,
                ..InstanceSpec::new("workload", "0.1.0")
            },
        )
        .unwrap();
    let mut off = Core::new(&engine(false), &core, false).await;
    let mut on = Core::new(&on, &core, true).await;
    println!(
        "median of {SAMPLES}; inputs: orders {orders} B, log {log} B, markdown {markdown} B; \
         epoch tick {} ms",
        RuntimeConfig::default().epoch_tick.as_millis()
    );
    println!(
        "| Workload | Runs | Native | In Paddock | Ratio | Core, epoch off | Core, epoch on | Epoch ratio |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    for (id, name) in (0u32..).zip(WORKLOADS) {
        // Calibrate natively, then warm up every side and check it agrees.
        let (once, _) = native(&inputs, name, 1);
        let n = u32::try_from((TARGET.as_nanos() / once.as_nanos().max(1)).max(1)).unwrap();
        let (_, expected) = native(&inputs, name, n);
        assert_eq!(
            guest(&h, name, n).await.1,
            expected,
            "{name}: Paddock differs"
        );
        assert_eq!(off.time(id, n).await.1, expected, "{name}: core differs");
        assert_eq!(on.time(id, n).await.1, expected, "{name}: core differs");
        let mut samples: [Vec<Duration>; 4] = Default::default();
        for _ in 0..SAMPLES {
            samples[0].push(native(&inputs, name, n).0);
            samples[1].push(guest(&h, name, n).await.0);
            samples[2].push(off.time(id, n).await.0);
            samples[3].push(on.time(id, n).await.0);
        }
        let [a, b, c, d] = samples.map(median);
        println!(
            "| {name} | {n} | {} | {} | {:.2}x | {} | {} | {:.3} |",
            ms(a),
            ms(b),
            ratio(b, a),
            ms(c),
            ms(d),
            ratio(d, c)
        );
    }
}

fn main() {
    let component = std::fs::read(GUEST).unwrap_or_else(|e| panic!("{GUEST}: {e}"));
    let core = std::fs::read(CORE).unwrap_or_else(|e| panic!("{CORE}: {e}"));
    let tick = RuntimeConfig::default().epoch_tick;
    let on = engine(true);
    let stop = Arc::new(AtomicBool::new(false));
    let ticker = {
        let (engine, stop) = (on.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(tick);
                engine.increment_epoch();
            }
        })
    };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { tokio::spawn(run(component, core, on)).await })
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    ticker.join().unwrap();
}
