//! The engine, the epoch ticker and instance spawning.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig};

use crate::apps::{self, AppDeployment, Apps};
use crate::bindings::{PaddockApp, PaddockAppPre};
use crate::config::{MemoryProfile, RuntimeConfig};
use crate::deny;
use crate::error::{HostError, describe};
use crate::instance::{self, InstanceHandle};
use crate::links::LinkTable;
use crate::message::{CallError, Envelope};
use crate::pid::Pid;
use crate::pool::Pool;
use crate::registry::Registry;
use crate::services;
use crate::spec::InstanceSpec;
use crate::state::InstanceState;
use crate::supervisor::{self, SupervisorHandle, SupervisorSpec};
use crate::table::ProcessTable;
use paddock_bundle::manifest::LogLevel;

/// Hosts instances on one node.
///
/// Must be used from a multi-threaded tokio runtime: with `current_thread`,
/// timers and other tasks starve while a guest yields every epoch tick
/// (spike S1).
#[derive(Debug, Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

pub(crate) struct RuntimeInner {
    pub(crate) engine: Engine,
    pub(crate) table: ProcessTable,
    pub(crate) registry: Registry,
    pub(crate) links: LinkTable,
    pools: RwLock<HashMap<String, Arc<Pool>>>,
    pub(crate) services: services::Shared,
    next_id: AtomicU64,
    config: RuntimeConfig,
    _ticker: Ticker,
}

impl RuntimeInner {
    /// An instance of the running app called `name`, round-robin.
    pub(crate) fn pool_member(&self, name: &str) -> Option<InstanceHandle> {
        let pool = self
            .pools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()?;
        pool.pick(None)
    }
}

impl fmt::Debug for RuntimeInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeInner")
            .field("config", &self.config)
            .field("instances", &self.table.len())
            .finish_non_exhaustive()
    }
}

/// A compiled component, linked against the host and ready to instantiate.
#[derive(Clone)]
pub struct LoadedApp {
    pub(crate) pre: PaddockAppPre<InstanceState>,
    stubbed: Arc<Vec<String>>,
}

impl LoadedApp {
    /// Imported interfaces no host here provides, linked to deny stubs.
    #[must_use]
    pub fn stubbed_imports(&self) -> &[String] {
        &self.stubbed
    }
}

impl fmt::Debug for LoadedApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedApp")
            .field("stubbed", &self.stubbed)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// Create the engine and start the epoch ticker.
    ///
    /// # Errors
    ///
    /// [`HostError::Engine`] if wasmtime rejects the configuration (for
    /// example when the pool cannot reserve its address space) or the epoch
    /// thread cannot start.
    pub fn new(config: RuntimeConfig) -> Result<Self, HostError> {
        let engine =
            Engine::new(&engine_config(&config)).map_err(|e| HostError::Engine(describe(&e)))?;
        // Fail now, not at the first load, if the host interfaces cannot link.
        base_linker(&engine).map_err(|e| HostError::Engine(describe(&e)))?;
        let services = services::Shared::new(&config)?;
        let ticker = Ticker::start(engine.clone(), config.epoch_tick)
            .map_err(|e| HostError::Engine(format!("cannot start the epoch thread: {e}")))?;
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                engine,
                table: ProcessTable::default(),
                registry: Registry::default(),
                links: LinkTable::default(),
                pools: RwLock::default(),
                services,
                next_id: AtomicU64::new(1),
                config,
                _ticker: ticker,
            }),
        })
    }

    /// Compile a component and link it against the host. Imported
    /// interfaces the host does not provide are linked to deny stubs (see
    /// [`LoadedApp::stubbed_imports`]).
    ///
    /// # Errors
    ///
    /// [`HostError::Load`] if the bytes are not a valid component or it
    /// cannot link, for example a provided interface at an incompatible
    /// version.
    pub fn load(&self, component: &[u8]) -> Result<LoadedApp, HostError> {
        let load = |e: wasmtime::Error| HostError::Load(describe(&e));
        let engine = &self.inner.engine;
        let component = Component::new(engine, component).map_err(load)?;
        let mut linker = base_linker(engine).map_err(load)?;
        let stubbed = deny::link_stubs(&mut linker, engine, &component).map_err(load)?;
        let pre = linker.instantiate_pre(&component).map_err(load)?;
        Ok(LoadedApp {
            pre: PaddockAppPre::new(pre).map_err(load)?,
            stubbed: Arc::new(stubbed),
        })
    }

    /// Start a new instance (incarnation 1). It begins in `Starting`; watch
    /// [`InstanceHandle::state`] or await [`InstanceHandle::wait`].
    ///
    /// # Errors
    ///
    /// [`HostError::InstanceLimit`] when every slot is in use, and
    /// [`HostError::MemoryAboveProfile`] when `memory_max` exceeds what the
    /// node's memory profile reserves.
    pub fn spawn(&self, app: &LoadedApp, spec: InstanceSpec) -> Result<InstanceHandle, HostError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.start(app, spec, id, 1)
    }

    /// Start the next incarnation of an instance that has exited: same id,
    /// incarnation one higher.
    ///
    /// # Errors
    ///
    /// As for [`Runtime::spawn`], and [`HostError::StillRunning`] if an
    /// instance with that id is still alive.
    pub fn respawn(
        &self,
        app: &LoadedApp,
        spec: InstanceSpec,
        previous: Pid,
    ) -> Result<InstanceHandle, HostError> {
        self.start(
            app,
            spec,
            previous.id,
            previous.incarnation.saturating_add(1),
        )
    }

    /// Start a supervisor and its children (none yet for
    /// `simple_one_for_one`; add them with
    /// [`SupervisorHandle::start_child`]).
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidSupervisor`] for an invalid spec, and the errors
    /// of [`Runtime::spawn`] if a child cannot start; children already
    /// started are then killed.
    pub fn start_supervisor(&self, spec: SupervisorSpec) -> Result<SupervisorHandle, HostError> {
        supervisor::start(self, spec)
    }

    /// The live instance with exactly this pid. `None` for a dead pid or an
    /// earlier incarnation.
    #[must_use]
    pub fn lookup(&self, pid: Pid) -> Option<InstanceHandle> {
        self.inner.table.lookup(pid)
    }

    /// The live instance registered under `name`. A name no instance holds
    /// but that names a running app resolves to one of the app's instances,
    /// round-robin.
    #[must_use]
    pub fn whereis(&self, name: &str) -> Option<InstanceHandle> {
        match self.inner.registry.whereis(name) {
            Some(pid) => self.lookup(pid),
            None => self.inner.pool_member(name),
        }
    }

    /// Start a set of apps in dependency order, each under its own root
    /// supervisor. An app that cannot start, or whose dependency is not
    /// running, is marked failed and the rest still start.
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidApps`] before anything starts, for a missing or
    /// mismatched dependency, a cycle, a duplicate name, or a supervision
    /// child naming an app that is not deployed.
    pub async fn start_apps(&self, deployments: Vec<AppDeployment>) -> Result<Apps, HostError> {
        apps::start(self, deployments).await
    }

    /// Override the log level of every instance of `app` from now on, or
    /// return to each instance's own level with `None`.
    pub fn set_log_level(&self, app: &str, level: Option<LogLevel>) {
        self.inner.services.logging.set_override(app, level);
    }

    /// The log level override for `app`, if one is set.
    #[must_use]
    pub fn log_level(&self, app: &str) -> Option<LogLevel> {
        self.inner.services.logging.override_for(app)
    }

    pub(crate) fn add_pool(&self, name: &str, pool: Arc<Pool>) {
        self.inner
            .pools
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_string(), pool);
    }

    pub(crate) fn remove_pool(&self, name: &str) {
        self.inner
            .pools
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
    }

    /// Cast to a pid.
    ///
    /// # Errors
    ///
    /// [`CallError::Noproc`] for a dead pid or an earlier incarnation, and as
    /// for [`InstanceHandle::cast`].
    pub fn cast(&self, to: Pid, msg: Envelope) -> Result<(), CallError> {
        self.lookup(to).ok_or(CallError::Noproc)?.cast(msg)
    }

    /// Call a pid.
    ///
    /// # Errors
    ///
    /// [`CallError::Noproc`] for a dead pid or an earlier incarnation, and as
    /// for [`InstanceHandle::call`].
    pub async fn call(
        &self,
        to: Pid,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, CallError> {
        self.lookup(to)
            .ok_or(CallError::Noproc)?
            .call(msg, timeout)
            .await
    }

    /// Every live instance, ordered by pid.
    #[must_use]
    pub fn instances(&self) -> Vec<InstanceHandle> {
        self.inner.table.all()
    }

    /// Number of live instances.
    #[must_use]
    pub fn instance_count(&self) -> usize {
        self.inner.table.len()
    }

    fn start(
        &self,
        app: &LoadedApp,
        spec: InstanceSpec,
        id: u64,
        incarnation: u32,
    ) -> Result<InstanceHandle, HostError> {
        if let Some(p) = spec.preopens.iter().find(|p| !p.host.is_dir()) {
            return Err(HostError::Preopen(format!(
                "{} (for {}) is not a directory",
                p.host.display(),
                p.guest
            )));
        }
        let limit = self.inner.config.memory_profile.memory_max();
        if spec.memory_max > limit {
            return Err(HostError::MemoryAboveProfile {
                requested: spec.memory_max,
                limit,
            });
        }
        let pid = Pid {
            node: self.inner.config.node,
            id,
            incarnation,
        };
        let (handle, task) = instance::prepare(pid, &spec);
        let capacity = usize::try_from(self.inner.config.max_instances).unwrap_or(usize::MAX);
        self.inner
            .table
            .insert(handle.clone(), capacity)
            .map_err(|holder| match holder {
                Some(pid) => HostError::StillRunning(pid),
                None => HostError::InstanceLimit(self.inner.config.max_instances),
            })?;
        tokio::spawn(task.run(self.inner.clone(), app.clone(), spec));
        Ok(handle)
    }
}

/// The host interfaces every app is linked against.
fn base_linker(engine: &Engine) -> wasmtime::Result<Linker<InstanceState>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    PaddockApp::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)?;
    services::link(&mut linker)?;
    Ok(linker)
}

fn engine_config(config: &RuntimeConfig) -> Config {
    let n = config.max_instances;
    let mut pool = PoolingAllocationConfig::default();
    // Per-component counts from S2's Rust guests, with headroom.
    pool.total_component_instances(n)
        .total_core_instances(n.saturating_mul(4))
        .total_memories(n.saturating_mul(2))
        .total_tables(n.saturating_mul(4))
        .max_core_instances_per_component(8)
        .max_memories_per_component(4)
        .max_tables_per_component(8)
        // Each live Store holds a fiber stack (S2).
        .total_stacks(n)
        .max_memory_size(usize::try_from(config.memory_profile.memory_max()).unwrap_or(usize::MAX));
    let mut wasm = Config::new();
    wasm.epoch_interruption(true)
        .allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    if config.memory_profile == MemoryProfile::Dense {
        wasm.memory_reservation(128 << 20)
            .memory_guard_size(64 << 10)
            .memory_reservation_for_growth(0);
    }
    wasm
}

/// Advances the engine's epoch on a dedicated OS thread.
///
/// Not a tokio task: guests yield only when the epoch advances, so if every
/// worker were running a spinning guest, a ticker task would never be
/// scheduled and nothing would yield again.
#[derive(Debug)]
struct Ticker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Ticker {
    fn start(engine: Engine, tick: Duration) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("paddock-epoch".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(tick);
                    engine.increment_epoch();
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
