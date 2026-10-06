//! Spike S2: what does an instance cost?
//!
//! Uses the S5 counter component (Rust, wasm32-wasip2, WASI linked). For each
//! allocator (on-demand, pooling) and each count N it instantiates N
//! instances, each in its own Store, keeps them alive, and records:
//!   - instantiation time per instance (Store + WasiCtx + instantiate + init)
//!   - resident (VmRSS) and virtual (VmSize) memory added per instance
//!   - host -> guest call cost (handle-call "inc", called directly)
//!   - actor round trip cost (mpsc + oneshot + handle-call), as Paddock would do it
//!
//! usage: s2-bench <counter.wasm> [ondemand|pooling] [counts...]

use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Error, InstanceAllocationStrategy, PoolingAllocationConfig, Result, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "../s5/wit",
    world: "app",
    exports: { default: async },
});

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

fn proc_status(field: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn engine(strategy: &str, max_instances: u32) -> Result<Engine> {
    let mut config = Config::new();
    config.epoch_interruption(true);
    if strategy == "pooling-small" {
        // Small reservations: bounds checks come back, address space shrinks.
        config.memory_reservation(128 << 20).memory_guard_size(64 << 10).memory_reservation_for_growth(0);
    }
    if strategy.starts_with("pooling") {
        let mut pool = PoolingAllocationConfig::default();
        pool.total_component_instances(max_instances)
            .total_core_instances(max_instances * 4)
            .total_memories(max_instances * 2)
            .total_tables(max_instances * 4)
            .max_memory_size(64 << 20)
            .max_core_instances_per_component(8)
            .max_memories_per_component(4)
            .max_tables_per_component(8)
            // Each live Store keeps an async fiber stack; the pool must cover them.
            .total_stacks(max_instances);
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    }
    Engine::new(&config)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() as f64 - 1.0) * p) as usize]
}

async fn measure(wasm: &str, strategy: &str, n: usize) -> Result<()> {
    // Baseline before the engine exists: pooling reserves its address space up front.
    let vsz_before_engine = proc_status("VmSize:");
    let engine = engine(strategy, n as u32 + 16)?;
    let vsz_engine = proc_status("VmSize:").saturating_sub(vsz_before_engine);
    let component = Component::from_file(&engine, wasm)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    let pre = AppPre::new(linker.instantiate_pre(&component)?)?;

    let rss0 = proc_status("VmRSS:");
    let vsz0 = proc_status("VmSize:");
    let mut instances = Vec::with_capacity(n);
    let mut times = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let state = State { wasi: WasiCtx::builder().build(), table: ResourceTable::new() };
        let mut store = Store::new(&engine, state);
        store.set_epoch_deadline(u64::MAX / 2);
        let app = pre.instantiate_async(&mut store).await?;
        app.call_init(&mut store).await?.map_err(Error::msg)?;
        times.push(t.elapsed());
        instances.push((store, app));
    }
    // Touch every instance once so lazily-faulted pages are counted.
    for (store, app) in instances.iter_mut() {
        app.call_handle_call(&mut *store, "inc").await?;
    }
    let rss = proc_status("VmRSS:").saturating_sub(rss0);
    let vsz = proc_status("VmSize:").saturating_sub(vsz0) + vsz_engine;
    times.sort();
    let total: Duration = times.iter().sum();

    // Direct host -> guest call cost on one instance.
    let calls = 100_000;
    let (store, app) = &mut instances[0];
    let t = Instant::now();
    for _ in 0..calls {
        app.call_handle_call(&mut *store, "inc").await?;
    }
    let direct = t.elapsed() / calls;

    println!(
        "{strategy:<9} n={n:<5} instantiate mean {:>6.1} us  p50 {:>6.1} us  p99 {:>7.1} us | RSS/inst {:>7.1} KiB  VSZ/inst {:>9.1} MiB | call {:>5} ns",
        total.as_secs_f64() * 1e6 / n as f64,
        percentile(&times, 0.5).as_secs_f64() * 1e6,
        percentile(&times, 0.99).as_secs_f64() * 1e6,
        rss as f64 / n as f64,
        vsz as f64 / 1024.0 / n as f64,
        direct.as_nanos(),
    );

    // Actor round trip: the way Paddock delivers a call.
    if n == 1 {
        let (store, app) = instances.pop().unwrap();
        let (tx, mut rx) = mpsc::channel::<oneshot::Sender<String>>(64);
        let actor = tokio::spawn(async move {
            let (mut store, app) = (store, app);
            while let Some(reply) = rx.recv().await {
                let r = app.call_handle_call(&mut store, "inc").await.unwrap_or_default();
                let _ = reply.send(r);
            }
        });
        let round_trips = |tx: mpsc::Sender<oneshot::Sender<String>>| async move {
            let t = Instant::now();
            for _ in 0..calls {
                let (rtx, rrx) = oneshot::channel();
                tx.send(rtx).await.map_err(|_| Error::msg("actor gone"))?;
                rrx.await.map_err(|_| Error::msg("no reply"))?;
            }
            Ok::<_, Error>(t.elapsed() / calls)
        };
        let cross = round_trips(tx.clone()).await?;
        let same = tokio::spawn(round_trips(tx.clone())).await??;
        println!(
            "{strategy:<9} actor round trip (mpsc + oneshot + call): same worker {} ns, from another thread {} ns",
            same.as_nanos(),
            cross.as_nanos()
        );
        drop(tx);
        actor.await?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let wasm = args.next().ok_or_else(|| Error::msg("usage: s2-bench <counter.wasm> [ondemand|pooling] [counts...]"))?;
    let strategy = args.next().unwrap_or_else(|| "pooling".into());
    let counts: Vec<usize> = args.filter_map(|a| a.parse().ok()).collect();
    let counts = if counts.is_empty() { vec![1, 100, 1000, 5000] } else { counts };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build()?;
    rt.block_on(async move {
        for n in counts {
            measure(&wasm, &strategy, n).await?;
        }
        Ok(())
    })
}
