//! Spike S5: snapshot upgrade end to end.
//!
//! An instance actor owns a mailbox (its identity: the "pid") and a Store.
//! Upgrades are messages in the same mailbox, so they run between callbacks
//! and everything sent meanwhile queues. The actor:
//!   1. asks the old instance for `version` and `save-snapshot`
//!   2. instantiates the new (pre-compiled) component in a fresh Store
//!   3. calls `load-snapshot(old-version, bytes)` on it
//!   4. swaps on success; on any failure keeps the old instance (revert)
//! Four concurrent senders keep incrementing the counter throughout. If no
//! message is lost or duplicated, the final count equals the number sent.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Error, Result, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "../wit",
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

enum Msg {
    Call { msg: String, reply: oneshot::Sender<Result<String, String>> },
    Upgrade { component: Component, label: String, reply: oneshot::Sender<Result<String, String>> },
}

struct Host {
    engine: Engine,
    linker: Linker<State>,
}

impl Host {
    async fn instantiate(&self, component: &Component) -> Result<(Store<State>, App)> {
        let state = State { wasi: WasiCtx::builder().inherit_stderr().build(), table: ResourceTable::new() };
        let mut store = Store::new(&self.engine, state);
        let app = App::instantiate_async(&mut store, component, &self.linker).await?;
        Ok((store, app))
    }

    /// Steps 1-4 above. Returns the new instance, or an error with the old one untouched.
    async fn upgrade(&self, old: &mut (Store<State>, App), component: &Component) -> Result<(Store<State>, App)> {
        let (store, app) = old;
        let from = app.call_version(&mut *store).await?;
        let snapshot = app.call_save_snapshot(&mut *store).await?;
        let (mut new_store, new_app) = self.instantiate(component).await?;
        new_app
            .call_load_snapshot(&mut new_store, &from, &snapshot)
            .await?
            .map_err(|e| Error::msg(format!("load-snapshot refused: {e}")))?;
        Ok((new_store, new_app))
    }
}

fn spawn_instance(host: Arc<Host>, mut current: (Store<State>, App)) -> mpsc::Sender<Msg> {
    let (tx, mut rx) = mpsc::channel::<Msg>(10_000);
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            match m {
                Msg::Call { msg, reply } => {
                    let (store, app) = &mut current;
                    let r = app.call_handle_call(&mut *store, &msg).await.map_err(|e| e.to_string());
                    let _ = reply.send(r);
                }
                Msg::Upgrade { component, label, reply } => {
                    let started = Instant::now();
                    let queued = rx.len();
                    let outcome = match host.upgrade(&mut current, &component).await {
                        Ok(next) => {
                            current = next;
                            let (store, app) = &mut current;
                            let v = app.call_version(&mut *store).await.unwrap_or_default();
                            Ok(format!("{label}: now v{v}"))
                        }
                        Err(e) => Err(format!("{label}: reverted ({})", e.root_cause())),
                    };
                    let pause = started.elapsed();
                    let _ = reply.send(outcome.map(|s| format!("{s}; paused {} us, {queued} msgs queued meanwhile", pause.as_micros())));
                }
            }
        }
    });
    tx
}

async fn call(tx: &mpsc::Sender<Msg>, msg: &str) -> Result<String, String> {
    let (reply, rx) = oneshot::channel();
    tx.send(Msg::Call { msg: msg.into(), reply }).await.map_err(|_| "noproc")?;
    rx.await.map_err(|_| "noproc".to_string())?
}

async fn upgrade(tx: &mpsc::Sender<Msg>, component: &Component, label: &str) -> Result<String, String> {
    let (reply, rx) = oneshot::channel();
    tx.send(Msg::Upgrade { component: component.clone(), label: label.into(), reply }).await.map_err(|_| "noproc")?;
    rx.await.map_err(|_| "noproc".to_string())?
}

async fn run(dir: String) -> Result<()> {
    let engine = Engine::new(&Config::new())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    let host = Arc::new(Host { engine: engine.clone(), linker });

    // Pre-compile every version up front: compilation is not part of the pause.
    let load = |v: &str| Component::from_file(&engine, format!("{dir}/counter_{v}.wasm"));
    let (v1, v2, v3) = (load("v1")?, load("v2")?, load("v3")?);

    let mut first = host.instantiate(&v1).await?;
    first.1.call_init(&mut first.0).await?.map_err(Error::msg)?;
    let pid = spawn_instance(host.clone(), first);
    println!("started: {:?}", call(&pid, "get").await);

    const SENDERS: usize = 4;
    const PER_SENDER: usize = 1000;
    let started = Instant::now();
    let mut senders = Vec::new();
    for _ in 0..SENDERS {
        let pid = pid.clone();
        senders.push(tokio::spawn(async move {
            let mut ok = 0;
            for _ in 0..PER_SENDER {
                if call(&pid, "inc").await.is_ok() {
                    ok += 1;
                }
                // Pace senders so upgrades land while increments are in flight.
                tokio::time::sleep(Duration::from_micros(500)).await;
            }
            ok
        }));
    }

    for (component, label, wait) in [(&v2, "v1 -> v2", 150), (&v3, "v2 -> v3 (broken)", 150), (&v2, "v2 -> v2 again", 150)] {
        tokio::time::sleep(Duration::from_millis(wait)).await;
        match upgrade(&pid, component, label).await {
            Ok(s) => println!("upgrade ok  : {s}"),
            Err(s) => println!("upgrade fail: {s}"),
        }
        println!("  state now : {:?}", call(&pid, "get").await);
    }

    let mut acked = 0;
    for s in senders {
        acked += s.await?;
    }
    println!(
        "senders done: {acked}/{} increments acknowledged in {} ms",
        SENDERS * PER_SENDER,
        started.elapsed().as_millis()
    );
    println!("final state : {:?}", call(&pid, "get").await);
    Ok(())
}

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).ok_or_else(|| Error::msg("usage: s5-host <dir with counter_v1.wasm ...>"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(async move { tokio::spawn(run(dir)).await? })
}
