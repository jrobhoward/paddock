//! The engine, the epoch ticker and instance spawning.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wasmtime::component::{HasSelf, Linker};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig};

use crate::apps::{self, AppDeployment, Apps};
use crate::bindings::{PaddockApp, PaddockAppPre};
use crate::compiled;
use crate::config::{HostObjects, MemoryProfile, RuntimeConfig};
use crate::delivery::{self, Origin};
use crate::deny;
use crate::error::{HostError, describe};
use crate::instance::{self, InstanceHandle};
use crate::links::{LinkInfo, LinkTable};
use crate::listener::{self, Listener, ListenerSpec, LoadedConnection};
use crate::message::{CallError, Envelope};
use crate::metrics::Metrics;
use crate::pid::Pid;
use crate::pool::Pool;
use crate::registry::Registry;
use crate::services::{self, Extension};
use crate::spec::InstanceSpec;
use crate::state::InstanceState;
use crate::supervisor::{self, SupervisorHandle, SupervisorSpec};
use crate::table::ProcessTable;
use crate::transport::liveness::{HeartbeatPath, Liveness, NodeEvent, PeerStatus};
use crate::transport::membership::{Member, Membership};
use crate::transport::{self, IncomingCalls, Packet, PendingCalls, Transport};
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
    /// Apps' metrics and the host's counters.
    pub(crate) metrics: Metrics,
    /// This node's failover role, for `hot-standby` instances.
    failover_role: RwLock<Option<String>>,
    pub(crate) extensions: Vec<Arc<dyn Extension>>,
    transport: std::sync::OnceLock<Arc<dyn Transport>>,
    pub(crate) pending_calls: PendingCalls,
    pub(crate) incoming_calls: IncomingCalls,
    pub(crate) liveness: Liveness,
    pub(crate) membership: Membership,
    pub(crate) failover: crate::failover::Shared,
    /// One permit per Store the pooling allocator has room for, held by
    /// each instance while it has a Store (not while passivated).
    pub(crate) slots: Arc<tokio::sync::Semaphore>,
    /// Signalled once for each instance that waits for a slot: one idle
    /// instance that may be passivated is, at once. Waiters are woken in
    /// the order they began waiting, so the one idle longest goes first.
    pub(crate) slot_wanted: tokio::sync::Notify,
    next_id: AtomicU64,
    config: RuntimeConfig,
    _ticker: Ticker,
}

impl RuntimeInner {
    /// This node's id, for pids.
    pub(crate) fn node(&self) -> u32 {
        self.config.node
    }

    /// This start of the node.
    pub(crate) fn creation(&self) -> u32 {
        self.config.creation
    }

    /// This node's failover role, if failover runs.
    pub(crate) fn failover_role(&self) -> Option<String> {
        self.failover_role
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The node's host-object limits.
    pub(crate) fn host_objects(&self) -> HostObjects {
        self.config.host_objects
    }

    /// The transport to other nodes, once set.
    pub(crate) fn transport(&self) -> Option<&Arc<dyn Transport>> {
        self.transport.get()
    }

    /// A fresh instance id.
    pub(crate) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The instance of the running app called `name` that `msg` routes to
    /// under its `instances.routing`, as for a local caller.
    pub(crate) fn route_app(&self, name: &str, msg: &Envelope) -> Option<InstanceHandle> {
        self.pools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()?
            .pick(Some(msg))
    }

    /// A live instance of the running app called `name`, round-robin. A
    /// member that has exited is skipped, though its supervisor may not have
    /// noticed yet: its exit notices go out first, and a watcher asking for
    /// the name again on its `down` must not get the dead pid back.
    pub(crate) fn pool_member(&self, name: &str) -> Option<InstanceHandle> {
        let pool = self
            .pools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()?;
        pool.pick_live(|h| self.table.lookup(h.pid()).is_some())
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
        Self::with_extensions(config, Vec::new())
    }

    /// Like [`Runtime::new`], with host services from outside this crate
    /// ([`services::extension`]).
    ///
    /// # Errors
    ///
    /// As for [`Runtime::new`], and [`HostError::Engine`] for an extension
    /// whose package is not a valid extension key, is served twice, or
    /// cannot link.
    pub fn with_extensions(
        config: RuntimeConfig,
        extensions: Vec<Arc<dyn Extension>>,
    ) -> Result<Self, HostError> {
        if !(1..=crate::pid::MAX_CREATION).contains(&config.creation) {
            return Err(HostError::Engine(format!(
                "creation {} is outside 1..={}",
                config.creation,
                crate::pid::MAX_CREATION
            )));
        }
        let mut packages = std::collections::BTreeSet::new();
        for ext in &extensions {
            let package = ext.package();
            if !paddock_bundle::manifest::is_extension_key(package) {
                return Err(HostError::Engine(format!(
                    "extension package '{package}' must be `namespace:package` outside wasi and paddock"
                )));
            }
            if !packages.insert(package.to_string()) {
                return Err(HostError::Engine(format!(
                    "extension package '{package}' is served twice"
                )));
            }
        }
        let engine =
            Engine::new(&engine_config(&config)).map_err(|e| HostError::Engine(describe(&e)))?;
        // Fail now, not at the first load, if the host interfaces cannot link.
        base_linker(&engine, &extensions).map_err(|e| HostError::Engine(describe(&e)))?;
        let services = services::Shared::new(&config)?;
        let ticker = Ticker::start(engine.clone(), config.epoch_tick)
            .map_err(|e| HostError::Engine(format!("cannot start the epoch thread: {e}")))?;
        let inner = Arc::new(RuntimeInner {
            engine,
            table: ProcessTable::default(),
            registry: Registry::default(),
            links: LinkTable::new(config.node),
            pools: RwLock::default(),
            services,
            metrics: Metrics::default(),
            failover_role: RwLock::default(),
            extensions,
            transport: std::sync::OnceLock::new(),
            pending_calls: PendingCalls::default(),
            incoming_calls: IncomingCalls::new(config.incoming_calls_per_peer),
            liveness: Liveness::new(config.heartbeat_interval, config.node_timeout),
            membership: Membership::new(Member::new(
                config.node,
                config.creation,
                config.address.clone(),
                config.role,
            )),
            failover: crate::failover::Shared::default(),
            slots: Arc::new(tokio::sync::Semaphore::new(
                usize::try_from(config.max_instances).unwrap_or(usize::MAX),
            )),
            slot_wanted: tokio::sync::Notify::new(),
            next_id: AtomicU64::new(crate::pid::instance_id(config.creation, 1)),
            config,
            _ticker: ticker,
        });
        inner.services.groups().attach(&inner);
        Ok(Self { inner })
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
        self.load_with(component, None)
    }

    /// [`Runtime::load`], keeping the compiled code in the file `cache`: a
    /// later load with the same `cache` (after a restart) reads it instead
    /// of compiling again. The caller names the file after the component's
    /// contents, so a different component never reads it. A file group or
    /// others can write, or one from another wasmtime version or engine
    /// configuration, is compiled again and replaced.
    ///
    /// # Errors
    ///
    /// As for [`Runtime::load`]; a cache that cannot be read or written is
    /// logged and skipped, not an error.
    pub fn load_cached(&self, component: &[u8], cache: &Path) -> Result<LoadedApp, HostError> {
        self.load_with(component, Some(cache))
    }

    fn load_with(&self, component: &[u8], cache: Option<&Path>) -> Result<LoadedApp, HostError> {
        let load = |e: wasmtime::Error| HostError::Load(describe(&e));
        let (pre, stubbed) = self.link(component, cache).map_err(load)?;
        Ok(LoadedApp {
            pre: PaddockAppPre::new(pre).map_err(load)?,
            stubbed: Arc::new(stubbed),
        })
    }

    /// Compile a connection-handler component
    /// (`paddock:connection/connection-handler`) and link it, as
    /// [`Runtime::load`] does for apps.
    ///
    /// # Errors
    ///
    /// As for [`Runtime::load`], including a component that does not export
    /// the connection handler.
    pub fn load_connection(&self, component: &[u8]) -> Result<LoadedConnection, HostError> {
        self.load_connection_with(component, None)
    }

    /// [`Runtime::load_connection`], keeping the compiled code in `cache`
    /// as [`Runtime::load_cached`] does.
    ///
    /// # Errors
    ///
    /// As for [`Runtime::load_connection`].
    pub fn load_connection_cached(
        &self,
        component: &[u8],
        cache: &Path,
    ) -> Result<LoadedConnection, HostError> {
        self.load_connection_with(component, Some(cache))
    }

    fn load_connection_with(
        &self,
        component: &[u8],
        cache: Option<&Path>,
    ) -> Result<LoadedConnection, HostError> {
        let load = |e: wasmtime::Error| HostError::Load(describe(&e));
        let (pre, stubbed) = self.link(component, cache).map_err(load)?;
        LoadedConnection::new(pre, stubbed).map_err(load)
    }

    fn link(
        &self,
        component: &[u8],
        cache: Option<&Path>,
    ) -> wasmtime::Result<(wasmtime::component::InstancePre<InstanceState>, Vec<String>)> {
        let engine = &self.inner.engine;
        let component = compiled::component(engine, component, cache)?;
        let extensions = &self.inner.extensions;
        let mut linker = base_linker(engine, extensions)?;
        let served: Vec<&str> = extensions.iter().map(|e| e.package()).collect();
        let stubbed = deny::link_stubs(&mut linker, engine, &component, &served)?;
        Ok((linker.instantiate_pre(&component)?, stubbed))
    }

    /// Bind a listener and hand each accepted connection to a new instance
    /// of `connection`, run with `spec`'s limits and services
    /// ([`crate::listener`]). Must be called inside the tokio runtime.
    ///
    /// # Errors
    ///
    /// [`HostError::Listen`] if the address cannot be bound.
    pub fn listen(
        &self,
        listener: ListenerSpec,
        connection: LoadedConnection,
        spec: InstanceSpec,
    ) -> Result<Listener, HostError> {
        listener::listen(&self.inner, listener, connection, spec)
    }

    /// Start a new instance (incarnation 1). It begins in `Starting`; watch
    /// [`InstanceHandle::state`] or await [`InstanceHandle::wait`].
    ///
    /// # Errors
    ///
    /// [`HostError::InstanceLimit`] when every slot is in use, and
    /// [`HostError::MemoryAboveProfile`] when `memory_max` exceeds what the
    /// node's memory profile reserves, and [`HostError::HostObjectsAboveNode`]
    /// when a host-object limit exceeds the node's.
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

    /// Start every extension's node-level work
    /// ([`Extension::start`](services::Extension::start)) against `apps`,
    /// each with its entry in `configs` (by package; null if none). Keep the
    /// result: stopping or dropping it ends their tasks.
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidApps`] for configuration naming an extension this
    /// runtime does not have, or one an extension refuses.
    pub fn start_extensions(
        &self,
        apps: &Apps,
        configs: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<services::StartedExtensions, HostError> {
        services::extension::start(&self.inner.extensions, apps, configs)
    }

    /// Override the log level of every instance of `app` from now on, or
    /// return to each instance's own level with `None`.
    pub fn set_log_level(&self, app: &str, level: Option<LogLevel>) {
        self.inner.services.logging.set_override(app, level);
        // Each instance works out its own effective level and tells its
        // guest (`log-level` host event).
        for instance in self.instances() {
            if instance.app() == app {
                instance.announce();
            }
        }
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
        delivery::cast(&self.inner, Origin::Host, to, msg)
    }

    /// Call the app called `app` on node `node`, from this node's host. That
    /// node's pool for the app picks the instance under its
    /// `instances.routing`, as for one of its own callers.
    ///
    /// # Errors
    ///
    /// [`CallError::Noconnection`] if `node` cannot be reached,
    /// [`CallError::Noproc`] if the app does not run there, and as for
    /// [`InstanceHandle::call`].
    pub async fn call_app_on(
        &self,
        node: u32,
        app: &str,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, CallError> {
        if node == self.inner.node() {
            let target = self.inner.route_app(app, &msg).ok_or(CallError::Noproc)?;
            return target.call(msg, timeout).await;
        }
        transport::call_app(&self.inner, node, app.to_string(), msg, timeout).await
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
        delivery::call(&self.inner, Origin::Host, to, msg, timeout).await
    }

    pub(crate) fn inner(&self) -> &Arc<RuntimeInner> {
        &self.inner
    }

    /// This node's id, carried in every local pid.
    #[must_use]
    pub fn node(&self) -> u32 {
        self.inner.node()
    }

    /// Carry frames for pids on other nodes through `transport`
    /// ([`crate::transport`]). Set once; without one, every other node is
    /// unreachable (`noconnection`).
    ///
    /// # Errors
    ///
    /// [`HostError::Engine`] if a transport is already set.
    pub fn set_transport(&self, transport: Arc<dyn Transport>) -> Result<(), HostError> {
        self.inner
            .transport
            .set(transport)
            .map_err(|_| HostError::Engine("a transport is already set".into()))
    }

    /// Handle a packet that node `from` sent to this node. Called by the
    /// transport; must run inside the tokio runtime.
    pub fn deliver(&self, from: u32, packet: Packet) {
        transport::deliver(&self.inner, from, packet);
    }

    /// Node `node` cannot be reached: the session with it ends, so links and
    /// monitors with its pids end with `noconnection` and calls waiting on
    /// it fail. Called by a transport that detects loss itself; heartbeats
    /// ([`transport::liveness`]) detect it otherwise.
    pub fn node_down(&self, node: u32) {
        transport::end_session(&self.inner, node);
    }

    /// Send heartbeats to `node` and watch it. Starts the heartbeat task on
    /// first use, so call it inside the tokio runtime.
    pub fn add_peer(&self, node: u32) {
        self.inner.liveness.add_peer(&self.inner, node);
    }

    /// Also beat every peer on `path`, reported as `name` in
    /// [`NodeEvent`]s.
    pub fn add_heartbeat_path(&self, name: &str, path: Arc<dyn HeartbeatPath>) {
        self.inner.liveness.add_path(name, path);
    }

    /// A beat from `node` carrying `payload` arrived on the extra path
    /// `path`. Called by the path's receiving side.
    pub fn heard(&self, node: u32, path: &str, payload: &[u8]) {
        self.inner.liveness.on_beat(node, path, payload);
    }

    /// Changes in what this node knows about others, from now on.
    #[must_use]
    pub fn node_events(&self) -> tokio::sync::broadcast::Receiver<NodeEvent> {
        self.inner.liveness.subscribe()
    }

    /// Join the cluster through `seeds` (node ids the transport reaches):
    /// each is told about this node and answers with the members it knows,
    /// and this node then joins each of those
    /// ([`transport::membership`]). Call inside the tokio runtime, after
    /// [`Runtime::set_transport`].
    pub fn join_cluster(&self, seeds: &[u32]) {
        transport::join(&self.inner, seeds);
    }

    /// Leave the cluster: every member is told, and every session ends.
    pub fn leave_cluster(&self) {
        transport::leave(&self.inner);
    }

    /// The members this node knows, itself included, by node id.
    #[must_use]
    pub fn members(&self) -> Vec<Member> {
        self.inner.membership.list()
    }

    /// What this node knows about `node`, if it watches it.
    #[must_use]
    pub fn peer_status(&self, node: u32) -> Option<PeerStatus> {
        self.inner.liveness.status(node)
    }

    /// `pid`'s links and monitors as this node records them; for a pid on
    /// another node, the entries this node keeps with it.
    #[must_use]
    pub fn links(&self, pid: Pid) -> LinkInfo {
        self.inner.links.info(pid)
    }

    /// Every live instance, ordered by pid.
    #[must_use]
    pub fn instances(&self) -> Vec<InstanceHandle> {
        self.inner.table.all()
    }

    /// Set this node's failover role (`primary`, `standby`, `down`) and tell
    /// every `hot-standby` instance ([`InstanceSpec::announce_role`]).
    pub fn set_failover_role(&self, role: Option<&str>) {
        *self
            .inner
            .failover_role
            .write()
            .unwrap_or_else(PoisonError::into_inner) = role.map(str::to_string);
        for instance in self.instances() {
            instance.announce();
        }
    }

    /// Every app's metrics and the host's, in the Prometheus text format.
    #[must_use]
    pub fn render_metrics(&self) -> String {
        self.inner.metrics.render(&self.instances())
    }

    /// Number of live instances, passivated or not.
    #[must_use]
    pub fn instance_count(&self) -> usize {
        self.inner.table.len()
    }

    /// Number of instances holding a Store now: those not passivated.
    #[must_use]
    pub fn store_count(&self) -> usize {
        let slots = usize::try_from(self.inner.config.max_instances).unwrap_or(usize::MAX);
        slots.saturating_sub(self.inner.slots.available_permits())
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
        self.inner
            .config
            .host_objects
            .grant(&spec.host_objects)
            .map_err(HostError::HostObjectsAboveNode)?;
        let pid = Pid {
            node: self.inner.config.node,
            id,
            incarnation,
        };
        let (handle, task) = instance::prepare(pid, &spec, self.inner.metrics.stats(&spec.app));
        let config = &self.inner.config;
        let processes = config.max_processes.unwrap_or(config.max_instances);
        let capacity = usize::try_from(processes).unwrap_or(usize::MAX);
        self.inner
            .table
            .insert(handle.clone(), capacity)
            .map_err(|holder| match holder {
                Some(pid) => HostError::StillRunning(pid),
                None => HostError::InstanceLimit(processes),
            })?;
        tokio::spawn(task.run(self.inner.clone(), app.clone(), spec));
        Ok(handle)
    }
}

/// The host interfaces every app is linked against.
fn base_linker(
    engine: &Engine,
    extensions: &[Arc<dyn Extension>],
) -> wasmtime::Result<Linker<InstanceState>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    PaddockApp::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)?;
    services::link(&mut linker)?;
    for ext in extensions {
        ext.link(&mut linker)?;
    }
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
                let mut next = Instant::now() + tick;
                while !flag.load(Ordering::Relaxed) {
                    // Parked rather than asleep, so `drop` wakes it at once
                    // even with a long tick.
                    let now = Instant::now();
                    if now < next {
                        std::thread::park_timeout(next - now);
                        continue;
                    }
                    engine.increment_epoch();
                    next = now + tick;
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
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
