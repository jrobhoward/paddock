//! Spike S1: does the gen_server callback model work on wasmtime's async
//! component bindings?
//!
//! Each instance is an actor task that owns a `Store` and drains a mailbox,
//! one `handle-call` at a time. Host imports are async, so a guest blocked in
//! `call` or `sleep` suspends without holding a thread. A per-callback
//! deadline is enforced with epoch interruption; the epoch callback yields to
//! the executor every tick so a spinning guest can't starve other instances.
//!
//! Everything runs on a tokio runtime with ONE worker thread on purpose: if
//! any instance blocked the thread, the concurrent probes below would stall.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use wasmtime::{Error, Result};
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, UpdateDeadline};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "../wit",
    world: "app",
    imports: { default: async | trappable },
    exports: { default: async },
});

const TICK: Duration = Duration::from_millis(10);

type Reply = Result<String, String>;

struct Request {
    msg: String,
    reply: oneshot::Sender<Reply>,
}

#[derive(Clone, Default)]
struct Registry(Arc<RwLock<HashMap<String, mpsc::Sender<Request>>>>);

impl Registry {
    fn lookup(&self, name: &str) -> Option<mpsc::Sender<Request>> {
        self.0.read().unwrap().get(name).cloned()
    }
}

/// Per-instance store data.
struct State {
    name: String,
    wasi: WasiCtx,
    table: ResourceTable,
    registry: Registry,
    /// When the current callback started.
    callback_start: Instant,
    /// Time the current callback has spent suspended in host imports. This is
    /// excluded from the deadline: waiting on another instance is not runaway
    /// guest code (the `call` timeout covers that case).
    host_time: Duration,
    deadline: Duration,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl paddock::spike::host::Host for State {
    async fn call(&mut self, target: String, msg: String, timeout_ms: u32) -> wasmtime::Result<Reply> {
        let started = Instant::now();
        let result = async {
            let Some(tx) = self.registry.lookup(&target) else {
                return Err("noproc".to_string());
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(Request { msg, reply: reply_tx }).await.map_err(|_| "noproc".to_string())?;
            match tokio::time::timeout(Duration::from_millis(timeout_ms.into()), reply_rx).await {
                Ok(Ok(reply)) => reply,
                // Callee dropped the reply sender without answering.
                Ok(Err(_)) => Err("noproc".to_string()),
                Err(_) => Err("timeout".to_string()),
            }
        }
        .await;
        self.host_time += started.elapsed();
        Ok(result)
    }

    async fn sleep(&mut self, ms: u32) -> wasmtime::Result<()> {
        let started = Instant::now();
        tokio::time::sleep(Duration::from_millis(ms.into())).await;
        self.host_time += started.elapsed();
        Ok(())
    }

    async fn log(&mut self, msg: String) -> wasmtime::Result<()> {
        println!("    [{}] {msg}", self.name);
        Ok(())
    }
}

struct Runtime {
    engine: Engine,
    component: Component,
    linker: Linker<State>,
    registry: Registry,
}

impl Runtime {
    fn new(wasm_path: &str) -> Result<Self> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config)?;
        let component = Component::from_file(&engine, wasm_path)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        App::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)?;

        // Epoch ticker: the only source of preemption.
        let ticker = engine.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(TICK);
                ticker.increment_epoch();
            }
        });

        Ok(Self { engine, component, linker, registry: Registry::default() })
    }

    async fn instantiate(&self, name: &str, deadline: Duration) -> Result<(Store<State>, App)> {
        let state = State {
            name: name.to_string(),
            wasi: WasiCtx::builder().inherit_stderr().build(),
            table: ResourceTable::new(),
            registry: self.registry.clone(),
            callback_start: Instant::now(),
            host_time: Duration::ZERO,
            deadline,
        };
        let mut store = Store::new(&self.engine, state);
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|ctx| {
            let s = ctx.data();
            let running = s.callback_start.elapsed().saturating_sub(s.host_time);
            if running > s.deadline {
                Ok(UpdateDeadline::Interrupt)
            } else {
                // Cooperative preemption: give other instances the thread.
                if std::env::var("TRACE_YIELD").is_ok() {
                    println!("      yield {} at {:?}", s.name, epoch_ms());
                }
                Ok(UpdateDeadline::Yield(1))
            }
        });
        let app = App::instantiate_async(&mut store, &self.component, &self.linker).await?;
        Ok((store, app))
    }

    /// Spawn an instance actor under a trivial "supervisor": on a trap the
    /// instance is discarded and rebuilt with a new incarnation.
    async fn spawn(self: &Arc<Self>, name: &str, deadline: Duration, hard_cap: Option<Duration>) -> Result<()> {
        let (tx, mut rx) = mpsc::channel::<Request>(64);
        self.registry.0.write().unwrap().insert(name.to_string(), tx);
        let rt = self.clone();
        let name = name.to_string();
        let (mut store, mut app) = rt.instantiate(&name, deadline).await?;
        tokio::spawn(async move {
            let mut incarnation = 1;
            while let Some(req) = rx.recv().await {
                if std::env::var("TRACE_RECV").is_ok() {
                    println!("      {name} recv {:?} at {:?}", req.msg, epoch_ms());
                }
                store.data_mut().callback_start = Instant::now();
                store.data_mut().host_time = Duration::ZERO;
                store.set_epoch_deadline(1);
                let call = app.call_handle_call(&mut store, &req.msg);
                let outcome = match hard_cap {
                    Some(cap) => match tokio::time::timeout(cap, call).await {
                        Ok(r) => r.map_err(|e| format!("crashed: {}", root_cause(&e))),
                        Err(_) => Err("killed: hard cap (future dropped)".to_string()),
                    },
                    None => call.await.map_err(|e| format!("crashed: {}", root_cause(&e))),
                };
                match outcome {
                    Ok(reply) => {
                        let _ = req.reply.send(Ok(reply));
                    }
                    Err(reason) => {
                        let _ = req.reply.send(Err(reason.clone()));
                        if reason.starts_with("killed") && std::env::var("REUSE_AFTER_DROP").is_ok() {
                            println!("    [{name}] keeping the store after a dropped call (REUSE_AFTER_DROP)");
                            continue;
                        }
                        incarnation += 1;
                        println!("    [{name}] {reason}; restarting as incarnation {incarnation}");
                        match rt.instantiate(&name, deadline).await {
                            Ok((s, a)) => (store, app) = (s, a),
                            Err(e) => {
                                println!("    [{name}] restart failed: {e:#}");
                                break;
                            }
                        }
                    }
                }
            }
        });
        Ok(())
    }

    async fn call(&self, target: &str, msg: &str, timeout: Duration) -> Reply {
        let tx = self.registry.lookup(target).ok_or("noproc")?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(Request { msg: msg.to_string(), reply: reply_tx }).await.map_err(|_| "noproc")?;
        tokio::time::timeout(timeout, reply_rx)
            .await
            .map_err(|_| "timeout".to_string())?
            .map_err(|_| "noproc".to_string())?
    }
}

fn epoch_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() % 100_000
}

fn root_cause(e: &Error) -> String {
    if let Some(trap) = e.downcast_ref::<wasmtime::Trap>() {
        return format!("trap {trap}");
    }
    e.root_cause().to_string().lines().next().unwrap_or_default().to_string()
}

/// Fire `count` echo calls at `target` one after another and report latency.
async fn probe(rt: Arc<Runtime>, target: &'static str, count: usize, gap: Duration) -> (usize, Duration) {
    let mut ok = 0;
    let mut worst = Duration::ZERO;
    for _ in 0..count {
        let t = Instant::now();
        if rt.call(target, "echo:x", Duration::from_secs(5)).await.is_ok() {
            ok += 1;
        }
        worst = worst.max(t.elapsed());
        if std::env::var("TRACE_PROBE").is_ok() {
            println!("      probe {} ms", t.elapsed().as_millis());
        }
        tokio::time::sleep(gap).await;
    }
    (ok, worst)
}

fn report(label: &str, started: Instant, reply: &Reply) {
    println!("  {label}: {reply:?} after {} ms", started.elapsed().as_millis());
}

async fn run(wasm: &str) -> Result<()> {
    let rt = Arc::new(Runtime::new(wasm)?);
    let deadline = Duration::from_millis(500);
    for name in ["a", "b", "probe", "spinner"] {
        rt.spawn(name, deadline, None).await?;
    }
    rt.spawn("capped", deadline, Some(Duration::from_millis(1000))).await?;
    let long = Duration::from_secs(10);

    println!("E1 call through the host (a -> b)");
    let t = Instant::now();
    report("a forward b echo", t, &rt.call("a", "forward:b:5000:echo:hi", long).await);

    println!("E2 suspended in host imports does not block the thread or count against the deadline");
    let t = Instant::now();
    let slow = tokio::spawn({
        let rt = rt.clone();
        async move { rt.call("a", "forward:b:5000:sleep:2000", long).await }
    });
    let (ok, worst) = tokio::spawn(probe(rt.clone(), "probe", 100, Duration::from_millis(15))).await?;
    println!("  probe during the 2 s wait: {ok}/100 ok, worst latency {} ms", worst.as_millis());
    report("a forward b sleep 2000 (a's deadline is 500 ms)", t, &slow.await?);

    println!("E3 runaway guest is preempted every tick and killed at its deadline");
    let t = Instant::now();
    let spin = tokio::spawn({
        let rt = rt.clone();
        async move { rt.call("spinner", "spin", long).await }
    });
    let (ok, worst) = tokio::spawn(probe(rt.clone(), "probe", 20, Duration::from_millis(10))).await?;
    println!("  probe while spinner spins: {ok}/20 ok, worst latency {} ms", worst.as_millis());
    report("spinner spin", t, &spin.await?);
    let t = Instant::now();
    report("spinner echo after restart", t, &rt.call("spinner", "echo:back", long).await);

    println!("E4 caller learns the callee died");
    let t = Instant::now();
    report("a forward spinner spin", t, &rt.call("a", "forward:spinner:5000:spin", long).await);

    println!("E5 call timeout; the late reply is dropped");
    let t = Instant::now();
    report("a forward b sleep 1500, timeout 300", t, &rt.call("a", "forward:b:300:sleep:1500", long).await);
    tokio::time::sleep(Duration::from_millis(1400)).await;
    let t = Instant::now();
    report("b echo afterwards", t, &rt.call("b", "echo:still-alive", long).await);

    println!("E6 guest panic is a trap; instance restarts");
    let t = Instant::now();
    report("b panic", t, &rt.call("b", "panic", long).await);
    let t = Instant::now();
    report("b echo after restart", t, &rt.call("b", "echo:fresh", long).await);

    println!("E7 hard cap: drop the call future while the guest is suspended in a host import");
    let t = Instant::now();
    report("capped sleep 3000 (cap 1000)", t, &rt.call("capped", "sleep:3000", long).await);
    let t = Instant::now();
    report("capped echo afterwards", t, &rt.call("capped", "echo:after-drop", long).await);
    let t = Instant::now();
    report("capped spin (deadline 500 < cap 1000)", t, &rt.call("capped", "spin", long).await);

    println!("E8 unknown target");
    let t = Instant::now();
    report("a forward nobody", t, &rt.call("a", "forward:nobody:1000:echo:x", long).await);
    Ok(())
}

fn main() -> Result<()> {
    let wasm = std::env::args().nth(1).ok_or_else(|| Error::msg("usage: s1-host <guest.wasm>"))?;
    // One worker thread for every instance: if anything blocks it, probes stall.
    let workers = std::env::var("WORKERS").ok().and_then(|w| w.parse().ok()).unwrap_or(1);
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(workers).enable_all().build()?;
    runtime.block_on(async move { tokio::spawn(async move { run(&wasm).await }).await? })
}
