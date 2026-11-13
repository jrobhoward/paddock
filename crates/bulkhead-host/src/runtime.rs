//! The node's runtime: instance spawning and the node-wide tables.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use crate::apps::{self, AppDeployment, Apps};
use crate::config::{HostObjects, NativeConfig, RuntimeConfig};
use crate::delivery::{self, Origin};
use crate::error::HostError;
use crate::instance::{self, InstanceEvent, InstanceHandle};
use crate::links::{LinkInfo, LinkTable};
use crate::listener::{self, Listener, ListenerSpec};
use crate::message::{CallError, Envelope};
use crate::metrics::Metrics;
use crate::native::{NativeCode, Program};
use crate::pid::Pid;
use crate::pool::Pool;
use crate::registry::Registry;
use crate::services::{self, Extension};
use crate::spec::InstanceSpec;
use crate::supervisor::{self, SupervisorHandle, SupervisorSpec};
use crate::table::ProcessTable;
use crate::transport::liveness::{HeartbeatPath, Liveness, NodeEvent, PeerStatus};
use crate::transport::membership::{Member, Membership};
use crate::transport::{self, IncomingCalls, Packet, PendingCalls, Transport};
use bulkhead_bundle::manifest::LogLevel;

/// Hosts instances on one node.
///
/// Must be used from a multi-threaded tokio runtime: native instances'
/// waits spin on it (spike S32).
#[derive(Debug, Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

pub(crate) struct RuntimeInner {
    pub(crate) table: ProcessTable,
    pub(crate) registry: Registry,
    pub(crate) links: LinkTable,
    pools: RwLock<HashMap<String, Arc<Pool>>>,
    pub(crate) services: services::Shared,
    /// Apps' metrics and the host's counters.
    pub(crate) metrics: Metrics,
    /// This node's failover role, for `hot-standby` instances.
    failover_role: RwLock<Option<String>>,
    /// The HTTP route table as JSON, for driver apps whose profile asks
    /// for it (ROADMAP DA1); `None` before an ingress sets one.
    http_routes: RwLock<Option<String>>,
    pub(crate) extensions: Vec<Arc<dyn Extension>>,
    transport: std::sync::OnceLock<Arc<dyn Transport>>,
    pub(crate) pending_calls: PendingCalls,
    pub(crate) incoming_calls: IncomingCalls,
    /// Credit for waited casts between nodes, both ways (ROADMAP I12).
    pub(crate) credits: crate::credit::Credits,
    pub(crate) liveness: Liveness,
    pub(crate) membership: Membership,
    pub(crate) failover: crate::failover::Shared,
    /// Moved singletons' state, for their first instance's start.
    pub(crate) handovers: crate::handover::Pending,
    /// A pair's version newer than this build, once replication applied
    /// one ([`Runtime::formats_outrun`]).
    pub(crate) formats_outrun: tokio::sync::watch::Sender<Option<u16>>,
    /// Instance exits and supervisor restarts, for whoever listens
    /// ([`Runtime::instance_events`]).
    pub(crate) instance_events: tokio::sync::broadcast::Sender<InstanceEvent>,
    next_id: AtomicU64,
    config: RuntimeConfig,
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

    /// The HTTP route table as JSON, once an ingress has set one.
    pub(crate) fn http_routes(&self) -> Option<String> {
        self.http_routes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The node's host-object limits.
    pub(crate) fn host_objects(&self) -> HostObjects {
        self.config.host_objects
    }

    /// How the node runs native-process apps.
    pub(crate) fn native_config(&self) -> &NativeConfig {
        &self.config.native
    }

    /// The transport to other nodes, once set.
    pub(crate) fn transport(&self) -> Option<&Arc<dyn Transport>> {
        self.transport.get()
    }

    /// `spec` with the driver profile node config gives its app, if any
    /// (ROADMAP DA1): its listeners and files, and its messaging ACL's
    /// `may_call` from the profile's `calls`.
    pub(crate) fn with_driver(&self, mut spec: InstanceSpec) -> InstanceSpec {
        if let Some(profile) = self.config.drivers.get(&spec.app) {
            spec.acl.may_call.clone_from(&profile.calls);
            spec.driver = Some(profile.clone());
        }
        spec
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

/// A native-process app's program, or an in-process test app.
#[derive(Clone)]
pub struct LoadedApp {
    code: Arc<Program>,
}

impl LoadedApp {
    /// The app's program.
    pub(crate) fn native(&self) -> &Arc<Program> {
        &self.code
    }
}

impl fmt::Debug for LoadedApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedApp").finish_non_exhaustive()
    }
}

impl Runtime {
    /// Create the runtime.
    ///
    /// # Errors
    ///
    /// [`HostError::NativeCgroup`] if
    /// `native.cgroup` is set but the node cannot move itself into it.
    pub fn new(config: RuntimeConfig) -> Result<Self, HostError> {
        Self::with_extensions(config, Vec::new())
    }

    /// Like [`Runtime::new`], with host services from outside this crate
    /// ([`services::extension`]).
    ///
    /// # Errors
    ///
    /// As for [`Runtime::new`], and [`HostError::Engine`] for an extension
    /// whose package is not a valid extension key or is served twice.
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
        if let Some(root) = &config.native.cgroup {
            if !cfg!(target_os = "linux") {
                return Err(HostError::NativeCgroup(format!(
                    "{}: cgroups are Linux's; on FreeBSD memory_max is RLIMIT_DATA \
                     until the privileged helper sets rctl rules (ROADMAP PS1)",
                    root.display()
                )));
            }
            crate::native::cgroup::prepare(root)
                .map_err(|e| HostError::NativeCgroup(format!("{}: {e}", root.display())))?;
        }
        let mut packages = std::collections::BTreeSet::new();
        for ext in &extensions {
            let package = ext.package();
            if !bulkhead_bundle::manifest::is_extension_key(package) {
                return Err(HostError::Engine(format!(
                    "extension package '{package}' must be `namespace:package` outside bulkhead"
                )));
            }
            if !packages.insert(package.to_string()) {
                return Err(HostError::Engine(format!(
                    "extension package '{package}' is served twice"
                )));
            }
        }
        let services = services::Shared::new(&config)?;
        let inner = Arc::new(RuntimeInner {
            table: ProcessTable::default(),
            registry: Registry::default(),
            links: LinkTable::new(config.node),
            pools: RwLock::default(),
            services,
            metrics: Metrics::default(),
            failover_role: RwLock::default(),
            http_routes: RwLock::default(),
            extensions,
            transport: std::sync::OnceLock::new(),
            pending_calls: PendingCalls::default(),
            incoming_calls: IncomingCalls::new(config.incoming_calls_per_peer),
            credits: crate::credit::Credits::new(config.cast_credit, config.cast_credit_bytes),
            liveness: Liveness::new(config.heartbeat_interval, config.node_timeout),
            membership: Membership::new(Member::new(
                config.node,
                config.creation,
                config.address.clone(),
                config.role,
            )),
            failover: crate::failover::Shared::default(),
            handovers: crate::handover::Pending::default(),
            formats_outrun: tokio::sync::watch::channel(None).0,
            instance_events: tokio::sync::broadcast::channel(1024).0,
            next_id: AtomicU64::new(crate::pid::instance_id(config.creation, 1)),
            config,
        });
        inner.services.groups().attach(&inner);
        Ok(Self { inner })
    }

    /// A native-process app whose program is `program`: its
    /// instances run it through `bulkhead-runner`. Nothing is checked here;
    /// a program that does not start fails each instance's start.
    #[must_use]
    pub fn load_native(&self, program: NativeCode) -> LoadedApp {
        LoadedApp {
            code: Arc::new(Program::Process(program)),
        }
    }

    /// An in-process test app (cargo feature `test-apps`): its instances
    /// run `A` in their own tasks, over the native protocol's frames
    /// ([`crate::in_process`]).
    #[cfg(feature = "test-apps")]
    #[must_use]
    pub fn load_in_process<A: crate::in_process::TestApp>(&self) -> LoadedApp {
        LoadedApp {
            code: Arc::new(Program::InProcess(
                crate::in_process::InProcessApp::of::<A>(),
            )),
        }
    }

    /// Bind a listener and hand each accepted connection to an instance of
    /// the native app `app`, which takes it in `handle_connection`
    /// ([`crate::listener`]). Must be called inside the tokio runtime.
    ///
    /// # Errors
    ///
    /// [`HostError::Listen`] if the address cannot be bound, or off Linux.
    pub fn listen_native(
        &self,
        listener: ListenerSpec,
        app: crate::AppHandle,
    ) -> Result<Listener, HostError> {
        listener::listen_native(&self.inner, listener, app)
    }

    /// Start a new instance (incarnation 1). It begins in `Starting`; watch
    /// [`InstanceHandle::state`] or await [`InstanceHandle::wait`].
    ///
    /// # Errors
    ///
    /// [`HostError::InstanceLimit`] when every slot is in use, and
    /// [`HostError::HostObjectsAboveNode`] when a host-object limit exceeds
    /// the node's.
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
        let lease = self.inner.failover.run_until();
        services::extension::start(&self.inner.extensions, apps, &lease, configs)
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

    /// Cast to a pid, waiting up to `timeout` for room if its mailbox is
    /// full, on this node or another (as a native app's `cast_timeout`):
    /// `Ok` means the message is in the target's mailbox. For embedders
    /// and tests; the admin API and the HTTP ingress do not wait.
    ///
    /// # Errors
    ///
    /// [`CallError::Timeout`] if no room came in time (the cast was not
    /// queued; between nodes, unless the answer itself was late), and as
    /// for [`Runtime::cast`].
    pub async fn cast_timeout(
        &self,
        to: Pid,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<(), CallError> {
        delivery::cast_wait(&self.inner, Origin::Host, to, msg, timeout).await
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

    /// Instance exits and supervisor restarts on this node, from now on. A
    /// listener that falls more than 1024 events behind loses the oldest
    /// (`RecvError::Lagged`).
    #[must_use]
    pub fn instance_events(&self) -> tokio::sync::broadcast::Receiver<InstanceEvent> {
        self.inner.instance_events.subscribe()
    }

    /// Tell listeners about `event`.
    pub(crate) fn instance_event(&self, event: InstanceEvent) {
        let _ = self.inner.instance_events.send(event);
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

    /// Give `path`, a directory the node wrote for `app`, to that app's
    /// user (ROADMAP PS2), through the privileged helper: read-only, to its
    /// group (the program's and `priv/` directories); read-write, the
    /// directory itself (a volume). Without the helper the app runs as the
    /// node's user and this does nothing.
    ///
    /// # Errors
    ///
    /// The helper's refusal, or the helper gone.
    pub async fn share_with_app(
        &self,
        app: &str,
        path: &std::path::Path,
        writable: bool,
    ) -> Result<(), String> {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(helper) = &self.inner.config.native.helper {
            let path = path
                .to_str()
                .ok_or_else(|| format!("{} is not UTF-8, as the helper needs", path.display()))?;
            return helper.share(app, path, writable).await;
        }
        let _ = (app, path, writable);
        Ok(())
    }

    /// Keep the HTTP route table (JSON, `GUEST_FORMATS.md`'s "Routes") and
    /// announce it to the driver apps whose profile asks for it (host event
    /// `routes`, ROADMAP DA1).
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub(crate) fn set_http_routes(&self, json: String) {
        *self
            .inner
            .http_routes
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(json);
        for instance in self.instances() {
            instance.announce();
        }
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
        let spec = self.inner.with_driver(spec);
        if let Some(p) = spec.preopens.iter().find(|p| !p.host.is_dir()) {
            return Err(HostError::Preopen(format!(
                "{} (for {}) is not a directory",
                p.host.display(),
                p.guest
            )));
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
