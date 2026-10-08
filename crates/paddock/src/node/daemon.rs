//! The node: a runtime, its apps, the deploy store and the operations the
//! admin API exposes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::api::{
    AppDetail, AppSummary, DeployResponse, Done, InstanceInfo, ListenerInfo, LogLine,
    RolloutParams, StateResponse, Target, UpgradeResponse,
};
use paddock_bundle::manifest::{AppManifest, UpgradeMode};
use paddock_bundle::{BundleError, OperatorPolicy, VerifiedBundle};
use paddock_host::services::Extension;
use paddock_host::{
    AppDeployment, AppHandle, AppStatus, Apps, CallError, Envelope, InstanceHandle, InstanceSpec,
    Listener, ListenerSpec, LoadedConnection, MemoryProfile as HostProfile, Pid, Role, Runtime,
    RuntimeConfig, UpgradeError, UpgradeOptions, UpgradeReport,
};
use serde_json::json;
use tokio::sync::Mutex;

use super::audit::Audit;
use super::config::{ClusterRole, MemoryProfile, NodeConfig};
use super::error::{NodeError, internal};
use super::logring::LogRing;
use super::store::{AppState, State, Store};

/// A running Paddock node.
#[derive(Debug)]
pub struct Node {
    config: NodeConfig,
    rt: Runtime,
    apps: Apps,
    store: Store,
    audit: Audit,
    logs: Arc<LogRing>,
    policy: OperatorPolicy,
    state: Mutex<NodeState>,
    /// Running connection-handler apps, by name. Locked only briefly,
    /// never across an await.
    connections: std::sync::Mutex<HashMap<String, ConnectionApp>>,
}

/// A connection-handler app this node runs: its listeners, and the
/// component they hand new connections to.
struct ConnectionApp {
    listeners: Vec<Listener>,
    current: ConnectionBuild,
    previous: Option<ConnectionBuild>,
}

impl std::fmt::Debug for ConnectionApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionApp")
            .field("listeners", &self.listeners)
            .finish_non_exhaustive()
    }
}

/// A compiled connection handler with the spec its instances run with.
#[derive(Clone)]
struct ConnectionBuild {
    manifest: AppManifest,
    connection: LoadedConnection,
    spec: InstanceSpec,
}

/// What a bundle runs as.
enum Runnable {
    /// Callbacks under a root supervisor.
    App(AppDeployment),
    /// An instance per connection on the app's listeners.
    Connection(ConnectionBuild),
}

#[derive(Debug, Default)]
struct NodeState {
    desired: State,
    info: HashMap<String, DeployInfo>,
    previous_info: HashMap<String, DeployInfo>,
    /// Apps that should run but could not be restored, with why.
    restore_errors: HashMap<String, String>,
}

/// What a deploy decided, kept for `status`.
#[derive(Debug, Clone, Default)]
struct DeployInfo {
    signer: String,
    granted: Vec<String>,
    denied: Vec<String>,
    stubbed: Vec<String>,
    warnings: Vec<String>,
}

/// A verified, compiled bundle ready to run.
struct Built {
    bundle: VerifiedBundle,
    run: Runnable,
    info: DeployInfo,
}

impl Built {
    fn manifest(&self) -> &AppManifest {
        match &self.run {
            Runnable::App(d) => &d.manifest,
            Runnable::Connection(c) => &c.manifest,
        }
    }

    fn name(&self) -> String {
        self.manifest().app.name.clone()
    }

    fn version(&self) -> String {
        self.manifest().app.version.to_string()
    }
}

fn refused(e: BundleError) -> NodeError {
    NodeError::Refused(e.to_string())
}

fn upgrade_error(e: UpgradeError) -> NodeError {
    match e {
        UpgradeError::UnknownApp(_) => NodeError::NotFound(e.to_string()),
        UpgradeError::InProgress(_) | UpgradeError::NoPrevious(_) => {
            NodeError::Conflict(e.to_string())
        }
        UpgradeError::Invalid(_) => NodeError::Invalid(e.to_string()),
        _ => NodeError::Refused(e.to_string()),
    }
}

/// [`upgrade_error`], with a pointer to `restart` when a snapshot-mode
/// rollout reverted and a restart-mode one might not.
fn rollout_error(e: UpgradeError, snapshot: bool, params: &RolloutParams) -> NodeError {
    if snapshot && !params.restart && matches!(e, UpgradeError::Reverted(_)) {
        return NodeError::Refused(format!(
            "{e}; --restart (API: restart=true) moves the instances by restart instead, discarding their in-memory state"
        ));
    }
    upgrade_error(e)
}

/// What a call or cast target resolved to.
enum Resolved {
    App(AppHandle),
    Instance(InstanceHandle),
}

/// The node error for a failed call or cast to `target`.
fn send_error(verb: &str, target: &Target, e: &CallError) -> NodeError {
    match e {
        CallError::Noproc => NodeError::NotFound(format!("{target} has no running instance")),
        CallError::TooLarge => NodeError::Invalid(e.to_string()),
        other => NodeError::Conflict(format!("{verb} to {target} failed: {other}")),
    }
}

fn status_text(status: &AppStatus) -> (String, Option<String>) {
    match status {
        AppStatus::Starting => ("starting".into(), None),
        AppStatus::Running => ("running".into(), None),
        AppStatus::Failed(reason) => ("failed".into(), Some(reason.to_string())),
        AppStatus::Stopped => ("stopped".into(), None),
    }
}

fn instance_info(h: &InstanceHandle) -> InstanceInfo {
    let pid = h.pid();
    InstanceInfo {
        pid: format!("{}.{}.{}", pid.node, pid.id, pid.incarnation),
        app: h.app().to_string(),
        version: h.version(),
        state: format!("{:?}", h.state()).to_lowercase(),
        queued: h.queued(),
        dropped: h.dropped_messages(),
        memory_bytes: h.memory_bytes(),
        restarts: h.restarts(),
        last_exit: h.last_exit().map(|r| r.to_string()),
    }
}

fn upgrade_response(report: &UpgradeReport) -> UpgradeResponse {
    UpgradeResponse {
        from: report.from.to_string(),
        to: report.to.to_string(),
        mode: format!("{:?}", report.mode).to_lowercase(),
        instances: report
            .instances
            .iter()
            .map(|p| format!("{}.{}.{}", p.node, p.id, p.incarnation))
            .collect(),
        pauses_us: report
            .pauses
            .iter()
            .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
            .collect(),
        approved: report
            .approved
            .iter()
            .map(|c| c.key().to_string())
            .collect(),
    }
}

fn options(params: &RolloutParams) -> UpgradeOptions {
    UpgradeOptions {
        approve: params.approve,
        batch_size: params.batch.unwrap_or(1),
        observe: Duration::from_millis(params.observe_ms.unwrap_or(0)),
        max_restarts: params.max_restarts.unwrap_or(0),
        restart: params.restart,
    }
}

impl Node {
    /// Open the deploy store, start the runtime and restore every app that
    /// was running. An app that can no longer be verified or started is
    /// reported as failed, not fatal.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] for an unreadable policy file;
    /// [`NodeError::Conflict`] if another node has `data_dir` open;
    /// [`NodeError::Internal`] if the store or runtime cannot start.
    pub async fn open(config: NodeConfig, logs: Arc<LogRing>) -> Result<Self, NodeError> {
        Self::open_with(config, logs, Vec::new()).await
    }

    /// Like [`Node::open`], with host services from outside `paddock-host`
    /// (`paddock_host::services::Extension`). Apps declare an extension by
    /// its package under `capabilities.extensions`; the policy grants it by
    /// the same name.
    ///
    /// # Errors
    ///
    /// As for [`Node::open`], and [`NodeError::Internal`] for an extension
    /// the runtime refuses (a bad or duplicate package name).
    pub async fn open_with(
        config: NodeConfig,
        logs: Arc<LogRing>,
        extensions: Vec<Arc<dyn Extension>>,
    ) -> Result<Self, NodeError> {
        let store = Store::open(&config.data_dir)?;
        let policy = match &config.policy {
            Some(path) => {
                OperatorPolicy::from_file(path).map_err(|e| NodeError::Invalid(e.to_string()))?
            }
            None => OperatorPolicy::default(),
        };
        if config.trusted_keys.is_empty() {
            tracing::warn!("no trusted keys configured: every deploy will be refused");
        }
        if !config.cluster.seeds.is_empty() {
            tracing::warn!("cluster.seeds is not used until the QUIC transport exists");
        }
        let desired = store.load_state()?;
        if config.cluster.role == ClusterRole::Witness && !desired.apps.is_empty() {
            return Err(NodeError::Invalid(format!(
                "this node is a witness, which runs no apps, but {} holds deployed apps",
                config.data_dir.display()
            )));
        }
        let creation = store.next_creation()?;
        let rt = Runtime::with_extensions(
            RuntimeConfig {
                node: config.node,
                max_instances: config.max_instances,
                memory_profile: match config.memory_profile {
                    MemoryProfile::Standard => HostProfile::Standard,
                    MemoryProfile::Dense => HostProfile::Dense,
                },
                epoch_tick: Duration::from_millis(config.epoch_tick_ms.max(1)),
                data_dir: Some(store.kv_dir()),
                heartbeat_interval: Duration::from_millis(config.cluster.heartbeat_interval_ms),
                node_timeout: Duration::from_millis(config.cluster.node_timeout_ms),
                creation,
                role: match config.cluster.role {
                    ClusterRole::Member => Role::Member,
                    ClusterRole::Witness => Role::Witness,
                },
                address: config.cluster.address.clone().unwrap_or_default(),
            },
            extensions,
        )
        .map_err(internal)?;
        let apps = rt.start_apps(Vec::new()).await.map_err(internal)?;
        let audit = Audit::new(store.audit_path());
        let node = Self {
            config,
            rt,
            apps,
            store,
            audit,
            logs,
            policy,
            state: Mutex::new(NodeState {
                desired,
                ..NodeState::default()
            }),
            connections: std::sync::Mutex::default(),
        };
        node.restore().await;
        Ok(node)
    }

    /// Start every app recorded as running, in deploy order (which already
    /// respects dependencies).
    async fn restore(&self) {
        let mut st = self.state.lock().await;
        let order = st.desired.order.clone();
        for name in order {
            let Some(app) = st.desired.apps.get(&name).cloned() else {
                continue;
            };
            if !app.running {
                continue;
            }
            match self.start_stored(&mut st, &name, &app).await {
                Ok(()) => self
                    .audit
                    .record("restore", json!({ "app": name, "version": app.current })),
                Err(e) => {
                    tracing::error!(app = %name, error = %e, "cannot restore app");
                    self.audit.record(
                        "restore_failed",
                        json!({ "app": name, "version": app.current, "reason": e.to_string() }),
                    );
                    st.restore_errors.insert(name, e.to_string());
                }
            }
        }
    }

    /// Start a deployed app from its stored bundles.
    async fn start_stored(
        &self,
        st: &mut NodeState,
        name: &str,
        app: &AppState,
    ) -> Result<(), NodeError> {
        let built = self
            .build(&self.store.bundle_path(name, &app.current))
            .await?;
        built
            .bundle
            .extract_priv(&self.store.priv_dir(name, &app.current))
            .map_err(internal)?;
        let handle = self.run(name, built.run).await?;
        st.info.insert(name.to_string(), built.info);
        if let Some(previous) = &app.previous {
            let prev = self.build(&self.store.bundle_path(name, previous)).await?;
            match (handle, prev.run) {
                (Some(handle), Runnable::App(d)) => handle.set_previous(Some(d)),
                (None, Runnable::Connection(c)) => {
                    if let Some(app) = self.connection_apps().get_mut(name) {
                        app.previous = Some(c);
                    }
                }
                // A previous version of the other kind cannot be rolled back
                // to (the world changed); keep none.
                _ => {}
            }
            st.previous_info.insert(name.to_string(), prev.info);
        }
        st.restore_errors.remove(name);
        Ok(())
    }

    /// Verify a bundle, check its imports against policy, and compile it.
    async fn build(&self, path: &Path) -> Result<Built, NodeError> {
        let keys = self.config.trusted_keys.clone();
        let owned = path.to_path_buf();
        let bundle = tokio::task::spawn_blocking(move || VerifiedBundle::open(&owned, &keys))
            .await
            .map_err(internal)?
            .map_err(refused)?;
        let grant = bundle.effective_grant(&self.policy).map_err(refused)?;
        let manifest = bundle.manifest.clone();
        let (name, version) = (manifest.app.name.clone(), manifest.app.version.to_string());
        let rt = self.rt.clone();
        let component = bundle.component.clone();
        let connection = manifest.component.world == paddock_bundle::CONNECTION_WORLD;
        let loaded = tokio::task::spawn_blocking(move || {
            if connection {
                rt.load_connection(&component).map(Loaded::Connection)
            } else {
                rt.load(&component).map(Loaded::App)
            }
        })
        .await
        .map_err(internal)?
        .map_err(|e| NodeError::Refused(e.to_string()))?;
        let mut spec = InstanceSpec::from_manifest(&manifest)
            .with_granted(&grant.granted)
            .with_granted_extensions(&grant.granted_extensions);
        if let Some(overrides) = self.config.config.get(&name) {
            spec.services.config.values.extend(overrides.clone());
        }
        let priv_dir = self.store.priv_dir(&name, &version);
        spec.resolve_preopens(
            &manifest,
            |v| self.config.volumes.get(v).cloned(),
            Some(&priv_dir),
        )
        .map_err(NodeError::Invalid)?;
        if let Some(p) = spec
            .preopens
            .iter()
            .find(|p| p.host != priv_dir && !p.host.is_dir())
        {
            return Err(NodeError::Invalid(format!(
                "volume directory {} (for {}) does not exist",
                p.host.display(),
                p.guest
            )));
        }
        let keys = |set: &std::collections::BTreeSet<paddock_bundle::manifest::Capability>,
                    extensions: &std::collections::BTreeSet<String>| {
            set.iter()
                .map(|c| c.key().to_string())
                .chain(extensions.iter().cloned())
                .collect::<Vec<_>>()
        };
        let info = DeployInfo {
            signer: manifest.provenance.signer_key.clone(),
            granted: keys(&grant.granted, &grant.granted_extensions),
            denied: keys(&grant.denied, &grant.denied_extensions),
            stubbed: match &loaded {
                Loaded::App(app) => app.stubbed_imports().to_vec(),
                Loaded::Connection(c) => c.stubbed_imports().to_vec(),
            },
            warnings: manifest
                .spec
                .unenforced_fields()
                .iter()
                .map(ToString::to_string)
                .collect(),
        };
        let run = match loaded {
            Loaded::App(app) => Runnable::App(AppDeployment {
                manifest,
                app,
                spec,
            }),
            Loaded::Connection(connection) => Runnable::Connection(ConnectionBuild {
                manifest,
                connection,
                spec,
            }),
        };
        Ok(Built { bundle, run, info })
    }

    fn connection_apps(&self) -> std::sync::MutexGuard<'_, HashMap<String, ConnectionApp>> {
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Start a built app: under a supervisor, or on its listeners. Returns
    /// the supervised app's handle.
    async fn run(&self, name: &str, run: Runnable) -> Result<Option<AppHandle>, NodeError> {
        match run {
            Runnable::App(deployment) => self
                .apps
                .add(deployment)
                .await
                .map(Some)
                .map_err(|e| NodeError::Invalid(e.to_string())),
            Runnable::Connection(build) => {
                let listeners = self.listen(&build)?;
                self.connection_apps().insert(
                    name.to_string(),
                    ConnectionApp {
                        listeners,
                        current: build,
                        previous: None,
                    },
                );
                Ok(None)
            }
        }
    }

    /// Bind every listener a connection handler declares, at the address
    /// node config gives it.
    fn listen(&self, build: &ConnectionBuild) -> Result<Vec<Listener>, NodeError> {
        let mut started: Vec<Listener> = Vec::new();
        for decl in &build.manifest.spec.listeners {
            let bound = self.config.listeners.get(&decl.name).ok_or_else(|| {
                NodeError::Invalid(format!(
                    "listener '{}' is not bound in node config ([listeners.{}])",
                    decl.name, decl.name
                ))
            });
            let listener = bound.and_then(|b| {
                let mut spec = ListenerSpec::new(&decl.name, b.address);
                spec.acceptors = b.acceptors;
                spec.max_connections = usize::try_from(decl.max_connections).unwrap_or(usize::MAX);
                spec.idle_timeout = Duration::from_millis(decl.idle_timeout_ms);
                self.rt
                    .listen(spec, build.connection.clone(), build.spec.clone())
                    .map_err(|e| NodeError::Invalid(e.to_string()))
            });
            match listener {
                Ok(l) => started.push(l),
                Err(e) => {
                    for l in &started {
                        l.stop();
                    }
                    return Err(e);
                }
            }
        }
        Ok(started)
    }

    /// Stop a connection handler's listeners, if it is one. Returns whether
    /// it was.
    fn stop_connections(&self, name: &str) -> bool {
        let Some(app) = self.connection_apps().remove(name) else {
            return false;
        };
        for l in &app.listeners {
            l.stop();
        }
        true
    }

    /// Whether `name` runs now, under a supervisor or on listeners.
    fn running(&self, name: &str) -> bool {
        self.apps.app(name).is_some() || self.connection_apps().contains_key(name)
    }

    fn summary(&self, name: &str, app: &AppState, restore_error: Option<&String>) -> AppSummary {
        let handle = self.apps.app(name);
        let connections = self
            .connection_apps()
            .get(name)
            .map(|c| c.listeners.iter().map(|l| l.stats().active).sum::<usize>());
        let (status, reason) = match (&handle, connections, restore_error) {
            (Some(h), _, _) => status_text(&h.status()),
            (None, Some(_), _) => ("running".into(), None),
            (None, None, Some(e)) => ("failed".into(), Some(e.clone())),
            (None, None, None) => ("stopped".into(), None),
        };
        AppSummary {
            name: name.to_string(),
            version: app.current.clone(),
            status,
            reason,
            instances: handle.map_or(connections.unwrap_or(0), |h| h.instances().len()),
            previous: app.previous.clone(),
        }
    }

    /// Deploy a new app from bundle bytes.
    ///
    /// # Errors
    ///
    /// [`NodeError::Refused`] for an untrusted, corrupt or non-conforming
    /// bundle; [`NodeError::Conflict`] if the app is already deployed;
    /// [`NodeError::Invalid`] for a missing dependency or volume.
    pub async fn deploy(&self, bytes: &[u8]) -> Result<DeployResponse, NodeError> {
        let mut st = self.state.lock().await;
        let upload = self.store.write_upload(bytes)?;
        let result = self.deploy_upload(&mut st, &upload).await;
        let _ = std::fs::remove_file(&upload);
        match &result {
            Ok(r) => self.audit.record(
                "deploy",
                json!({ "app": r.app.name, "version": r.app.version, "granted": r.granted, "denied": r.denied, "warnings": r.warnings }),
            ),
            Err(e) => self.audit.record("deploy_refused", json!({ "kind": e.kind(), "reason": e.to_string() })),
        }
        result
    }

    async fn deploy_upload(
        &self,
        st: &mut NodeState,
        upload: &Path,
    ) -> Result<DeployResponse, NodeError> {
        if self.config.cluster.role == ClusterRole::Witness {
            return Err(NodeError::Refused(
                "this node is a witness, which runs no apps".into(),
            ));
        }
        let built = self.build(upload).await?;
        let (name, version) = (built.name(), built.version());
        if st.desired.apps.contains_key(&name) {
            return Err(NodeError::Conflict(format!(
                "{name} is already deployed; upgrade it instead"
            )));
        }
        built
            .bundle
            .extract_priv(&self.store.priv_dir(&name, &version))
            .map_err(internal)?;
        self.store.keep_upload(upload, &name, &version)?;
        let info = built.info.clone();
        if let Err(e) = self.run(&name, built.run).await {
            self.store.remove_version(&name, &version);
            return Err(e);
        }
        let app = AppState {
            current: version,
            previous: None,
            running: true,
        };
        st.desired.order.push(name.clone());
        st.desired.apps.insert(name.clone(), app.clone());
        self.store.save_state(&st.desired)?;
        st.info.insert(name.clone(), info.clone());
        Ok(DeployResponse {
            app: self.summary(&name, &app, None),
            granted: info.granted,
            denied: info.denied,
            warnings: info.warnings,
        })
    }

    /// Upgrade a running app to bundle bytes.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`], [`NodeError::Conflict`] for a stopped app or
    /// the same version, and [`NodeError::Refused`] for a refused bundle or a
    /// rollout that reverted.
    pub async fn upgrade(
        &self,
        name: &str,
        bytes: &[u8],
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let mut st = self.state.lock().await;
        let upload = self.store.write_upload(bytes)?;
        let result = self.upgrade_upload(&mut st, name, &upload, params).await;
        let _ = std::fs::remove_file(&upload);
        match &result {
            Ok(r) => self.audit.record(
                "upgrade",
                json!({ "app": name, "from": r.from, "to": r.to, "mode": r.mode, "restart_requested": params.restart, "approved": r.approved, "granted": st.info.get(name).map(|i| i.granted.clone()) }),
            ),
            Err(e) => self.audit.record("upgrade_refused", json!({ "app": name, "kind": e.kind(), "reason": e.to_string() })),
        }
        result
    }

    async fn upgrade_upload(
        &self,
        st: &mut NodeState,
        name: &str,
        upload: &Path,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let current = st
            .desired
            .apps
            .get(name)
            .cloned()
            .ok_or_else(|| NodeError::NotFound(format!("{name} is not deployed")))?;
        if !current.running {
            return Err(NodeError::Conflict(format!(
                "{name} is stopped; start it first"
            )));
        }
        let built = self.build(upload).await?;
        let version = built.version();
        if built.name() != name {
            return Err(NodeError::Invalid(format!(
                "the bundle is {}, not {name}",
                built.name()
            )));
        }
        if version == current.current {
            return Err(NodeError::Conflict(format!(
                "{name} {version} is already running"
            )));
        }
        built
            .bundle
            .extract_priv(&self.store.priv_dir(name, &version))
            .map_err(internal)?;
        self.store.keep_upload(upload, name, &version)?;
        let info = built.info.clone();
        let result = match built.run {
            Runnable::Connection(build) => self
                .upgrade_connections(name, build, params)
                .map_err(|e| (e, false)),
            Runnable::App(deployment) => {
                let snapshot = deployment.manifest.spec.upgrade.mode == UpgradeMode::Snapshot;
                self.apps
                    .upgrade(name, deployment, &options(params))
                    .await
                    .map(|r| upgrade_response(&r))
                    .map_err(|e| (rollout_error(e, snapshot, params), true))
            }
        };
        match result {
            Ok(report) => {
                if let Some(dropped) = current.previous.as_ref().filter(|p| **p != version) {
                    self.store.remove_version(name, dropped);
                }
                let entry = st
                    .desired
                    .apps
                    .get_mut(name)
                    .ok_or_else(|| internal("app vanished"))?;
                entry.previous = Some(std::mem::replace(&mut entry.current, version));
                self.store.save_state(&st.desired)?;
                if let Some(old) = st.info.insert(name.to_string(), info) {
                    st.previous_info.insert(name.to_string(), old);
                }
                Ok(report)
            }
            Err((e, _)) => {
                if current.previous.as_deref() != Some(version.as_str()) {
                    self.store.remove_version(name, &version);
                }
                Err(e)
            }
        }
    }

    /// Upgrade a connection handler: new connections get `build`; open ones
    /// keep their version.
    fn upgrade_connections(
        &self,
        name: &str,
        build: ConnectionBuild,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let mut apps = self.connection_apps();
        let app = apps.get_mut(name).ok_or_else(|| {
            NodeError::Invalid(format!(
                "{name} is not a connection handler; the new version's world does not match"
            ))
        })?;
        let approved =
            paddock_host::validate_upgrade(&app.current.manifest, &build.manifest, params.approve)
                .map_err(upgrade_error)?;
        let names = |m: &AppManifest| {
            m.spec
                .listeners
                .iter()
                .map(|l| l.name.clone())
                .collect::<Vec<_>>()
        };
        if names(&app.current.manifest) != names(&build.manifest) {
            return Err(NodeError::Invalid(format!(
                "{name}'s listeners change between versions; remove and deploy it instead"
            )));
        }
        for listener in &app.listeners {
            listener.replace(build.connection.clone(), build.spec.clone());
        }
        let from = app.current.manifest.app.version.to_string();
        let to = build.manifest.app.version.to_string();
        app.previous = Some(std::mem::replace(&mut app.current, build));
        Ok(UpgradeResponse {
            from,
            to,
            mode: "new-connections".into(),
            instances: Vec::new(),
            pauses_us: Vec::new(),
            approved: approved.iter().map(|c| c.key().to_string()).collect(),
        })
    }

    /// Roll a connection handler back to its previous version, for new
    /// connections. `None` if `name` is not a connection handler.
    fn rollback_connections(&self, name: &str) -> Option<Result<UpgradeResponse, NodeError>> {
        let mut apps = self.connection_apps();
        let app = apps.get_mut(name)?;
        let Some(previous) = app.previous.take() else {
            return Some(Err(NodeError::Conflict(format!(
                "{name} has no previous version"
            ))));
        };
        for listener in &app.listeners {
            listener.replace(previous.connection.clone(), previous.spec.clone());
        }
        let from = app.current.manifest.app.version.to_string();
        let to = previous.manifest.app.version.to_string();
        app.previous = Some(std::mem::replace(&mut app.current, previous));
        Some(Ok(UpgradeResponse {
            from,
            to,
            mode: "new-connections".into(),
            instances: Vec::new(),
            pauses_us: Vec::new(),
            approved: Vec::new(),
        }))
    }

    /// Roll a running app back to its previous version.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`], [`NodeError::Conflict`] when there is no
    /// previous version, [`NodeError::Refused`] if the rollout reverted.
    pub async fn rollback(
        &self,
        name: &str,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let mut st = self.state.lock().await;
        if !st.desired.apps.contains_key(name) {
            return Err(NodeError::NotFound(format!("{name} is not deployed")));
        }
        let result = match self.rollback_connections(name) {
            Some(result) => result,
            None => {
                let snapshot = self.apps.app(name).and_then(|a| a.rollback_mode())
                    == Some(UpgradeMode::Snapshot);
                self.apps
                    .rollback(name, &options(params))
                    .await
                    .map(|r| upgrade_response_from(&r))
                    .map_err(|e| rollout_error(e, snapshot, params))
            }
        };
        match &result {
            Ok(r) => {
                if let Some(entry) = st.desired.apps.get_mut(name)
                    && let Some(previous) = entry.previous.take()
                {
                    entry.previous = Some(std::mem::replace(&mut entry.current, previous));
                }
                self.store.save_state(&st.desired)?;
                let (now, before) = (st.previous_info.remove(name), st.info.remove(name));
                if let Some(i) = now {
                    st.info.insert(name.to_string(), i);
                }
                if let Some(i) = before {
                    st.previous_info.insert(name.to_string(), i);
                }
                self.audit.record(
                    "rollback",
                    json!({ "app": name, "from": r.from, "to": r.to, "mode": r.mode, "restart_requested": params.restart }),
                );
            }
            Err(e) => self.audit.record(
                "rollback_refused",
                json!({ "app": name, "reason": e.to_string() }),
            ),
        }
        result
    }

    /// Stop an app; it stays deployed and does not start on restart.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`], or [`NodeError::Conflict`] if it is already
    /// stopped or a running app depends on it.
    pub async fn stop(&self, name: &str) -> Result<Done, NodeError> {
        let mut st = self.state.lock().await;
        let app = st
            .desired
            .apps
            .get_mut(name)
            .ok_or_else(|| NodeError::NotFound(format!("{name} is not deployed")))?;
        if !app.running {
            return Err(NodeError::Conflict(format!("{name} is already stopped")));
        }
        if !self.stop_connections(name) && self.apps.app(name).is_some() {
            self.apps
                .remove(name)
                .await
                .map_err(|e| NodeError::Conflict(e.to_string()))?;
        }
        app.running = false;
        st.restore_errors.remove(name);
        self.store.save_state(&st.desired)?;
        self.audit.record("stop", json!({ "app": name }));
        Ok(Done {
            message: format!("{name} stopped"),
        })
    }

    /// Start a stopped app from its stored bundle.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`], [`NodeError::Conflict`] if it is running,
    /// and the errors of a deploy.
    pub async fn start(&self, name: &str) -> Result<Done, NodeError> {
        let mut st = self.state.lock().await;
        let app = st
            .desired
            .apps
            .get(name)
            .cloned()
            .ok_or_else(|| NodeError::NotFound(format!("{name} is not deployed")))?;
        if self.running(name) {
            return Err(NodeError::Conflict(format!("{name} is already running")));
        }
        let started = self.start_stored(&mut st, name, &app).await;
        if let Err(e) = &started {
            self.audit.record(
                "start_refused",
                json!({ "app": name, "reason": e.to_string() }),
            );
        }
        started?;
        if let Some(entry) = st.desired.apps.get_mut(name) {
            entry.running = true;
        }
        self.store.save_state(&st.desired)?;
        self.audit
            .record("start", json!({ "app": name, "version": app.current }));
        Ok(Done {
            message: format!("{name} started"),
        })
    }

    /// Stop and undeploy an app, deleting its bundles. Its keyvalue data is
    /// kept.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`], or [`NodeError::Conflict`] if a running app
    /// depends on it.
    pub async fn remove(&self, name: &str) -> Result<Done, NodeError> {
        let mut st = self.state.lock().await;
        if !st.desired.apps.contains_key(name) {
            return Err(NodeError::NotFound(format!("{name} is not deployed")));
        }
        if !self.stop_connections(name) && self.apps.app(name).is_some() {
            self.apps
                .remove(name)
                .await
                .map_err(|e| NodeError::Conflict(e.to_string()))?;
        }
        self.store.remove_app(name);
        st.desired.apps.remove(name);
        st.desired.order.retain(|n| n != name);
        st.info.remove(name);
        st.previous_info.remove(name);
        st.restore_errors.remove(name);
        self.store.save_state(&st.desired)?;
        self.audit.record("remove", json!({ "app": name }));
        Ok(Done {
            message: format!("{name} removed"),
        })
    }

    /// Kill an instance; its supervisor restarts it.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] for a malformed pid, [`NodeError::NotFound`]
    /// for no live instance with it.
    pub async fn kill(&self, pid: &str) -> Result<Done, NodeError> {
        let pid: Pid = pid.parse().map_err(NodeError::Invalid)?;
        let handle = self
            .rt
            .lookup(pid)
            .ok_or_else(|| NodeError::NotFound(format!("no instance {pid}")))?;
        handle.kill();
        self.audit.record(
            "kill",
            json!({ "pid": pid.to_string(), "app": handle.app() }),
        );
        Ok(Done {
            message: format!("killed {pid}"),
        })
    }

    /// Every deployed app.
    pub async fn list(&self) -> Vec<AppSummary> {
        let st = self.state.lock().await;
        st.desired
            .order
            .iter()
            .filter_map(|n| {
                st.desired
                    .apps
                    .get(n)
                    .map(|a| self.summary(n, a, st.restore_errors.get(n)))
            })
            .collect()
    }

    /// One app in detail.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`].
    pub async fn status(&self, name: &str) -> Result<AppDetail, NodeError> {
        let st = self.state.lock().await;
        let app = st
            .desired
            .apps
            .get(name)
            .ok_or_else(|| NodeError::NotFound(format!("{name} is not deployed")))?;
        let info = st.info.get(name).cloned().unwrap_or_default();
        let handle = self.apps.app(name);
        Ok(AppDetail {
            summary: self.summary(name, app, st.restore_errors.get(name)),
            signer: info.signer,
            granted: info.granted,
            denied: info.denied,
            stubbed: info.stubbed,
            warnings: info.warnings,
            restarts: handle
                .as_ref()
                .and_then(AppHandle::supervisor)
                .map_or(0, paddock_host::SupervisorHandle::restart_count),
            instance_list: handle
                .map(|h| h.instances().iter().map(instance_info).collect())
                .unwrap_or_default(),
            listeners: self
                .connection_apps()
                .get(name)
                .map(|c| c.listeners.iter().map(listener_info).collect())
                .unwrap_or_default(),
        })
    }

    /// Every live instance.
    #[must_use]
    pub fn ps(&self) -> Vec<InstanceInfo> {
        self.rt.instances().iter().map(instance_info).collect()
    }

    /// An app's last `lines` log records.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`].
    pub async fn logs(&self, name: &str, lines: usize) -> Result<Vec<LogLine>, NodeError> {
        if !self.state.lock().await.desired.apps.contains_key(name) {
            return Err(NodeError::NotFound(format!("{name} is not deployed")));
        }
        Ok(self.logs.recent(name, lines))
    }

    /// Call an app, routed to one of its instances.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] for an app that is not running,
    /// [`NodeError::Invalid`] for a body over its `message_max_bytes`, and
    /// [`NodeError::Conflict`] for the other call failures.
    pub async fn call(
        &self,
        name: &str,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, NodeError> {
        self.call_to(&Target::App(name.to_string()), msg, timeout)
            .await
    }

    /// Call an app (through its pool), an instance by pid, or the instance
    /// holding a registered name.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] for an app that is not running, an unknown
    /// pid or an unregistered name, [`NodeError::Invalid`] for a bad pid or
    /// a body over `message_max_bytes`, and [`NodeError::Conflict`] for the
    /// other call failures.
    pub async fn call_to(
        &self,
        target: &Target,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, NodeError> {
        let result = match self.resolve(target)? {
            Resolved::App(app) => app.call(msg, timeout).await,
            Resolved::Instance(h) => h.call(msg, timeout).await,
        };
        result.map_err(|e| send_error("call", target, &e))
    }

    /// Cast to an app, an instance by pid, or a registered name.
    ///
    /// # Errors
    ///
    /// As for [`Node::call_to`]; a full mailbox is [`NodeError::Conflict`]
    /// (the cast is dropped and counted).
    pub fn cast_to(&self, target: &Target, msg: Envelope) -> Result<Done, NodeError> {
        match self.resolve(target)? {
            Resolved::App(app) => app.cast(msg),
            Resolved::Instance(h) => h.cast(msg),
        }
        .map_err(|e| send_error("cast", target, &e))?;
        Ok(Done {
            message: format!("cast to {target}"),
        })
    }

    fn resolve(&self, target: &Target) -> Result<Resolved, NodeError> {
        match target {
            Target::App(name) => self
                .apps
                .app(name)
                .map(Resolved::App)
                .ok_or_else(|| NodeError::NotFound(format!("{name} is not running"))),
            Target::Pid(pid) => {
                let pid: Pid = pid.parse().map_err(NodeError::Invalid)?;
                self.rt
                    .lookup(pid)
                    .map(Resolved::Instance)
                    .ok_or_else(|| NodeError::NotFound(format!("no instance {pid}")))
            }
            Target::Name(name) => self
                .rt
                .whereis(name)
                .map(Resolved::Instance)
                .ok_or_else(|| NodeError::NotFound(format!("no instance registered as {name}"))),
        }
    }

    /// Cast to an app, routed to one of its instances.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] for an app that is not running,
    /// [`NodeError::Invalid`] for a body over its `message_max_bytes`, and
    /// [`NodeError::Conflict`] for a full mailbox (the cast is dropped and
    /// counted).
    pub fn cast(&self, name: &str, msg: Envelope) -> Result<Done, NodeError> {
        self.cast_to(&Target::App(name.to_string()), msg)
    }

    /// An instance's state through `save-snapshot`.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] for a malformed pid, [`NodeError::NotFound`]
    /// for no live instance, and [`NodeError::Conflict`] if `save-snapshot`
    /// fails (the instance then restarts).
    pub async fn state(&self, pid: &str) -> Result<StateResponse, NodeError> {
        let pid: Pid = pid.parse().map_err(NodeError::Invalid)?;
        let handle = self
            .rt
            .lookup(pid)
            .ok_or_else(|| NodeError::NotFound(format!("no instance {pid}")))?;
        let bytes = handle
            .snapshot()
            .await
            .map_err(|e| NodeError::Conflict(format!("{pid}: {e}")))?;
        let shown = format!("{}.{}.{}", pid.node, pid.id, pid.incarnation);
        Ok(super::state_view::describe(&shown, &bytes))
    }

    /// Set every instance of an app to log at `level` (`trace` to
    /// `critical`), or with `reset`, at its own level again.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] for an app not deployed, [`NodeError::Invalid`]
    /// for an unknown level.
    pub async fn set_log_level(&self, name: &str, level: &str) -> Result<Done, NodeError> {
        if !self.state.lock().await.desired.apps.contains_key(name) {
            return Err(NodeError::NotFound(format!("{name} is not deployed")));
        }
        let parsed = if level == "reset" {
            None
        } else {
            Some(
                serde_json::from_value::<paddock_bundle::manifest::LogLevel>(json!(level)).map_err(|_| {
                    NodeError::Invalid(format!(
                        "unknown log level '{level}' (trace, debug, info, warn, error, critical, reset)"
                    ))
                })?,
            )
        };
        self.rt.set_log_level(name, parsed);
        self.audit
            .record("log_level", json!({ "app": name, "level": level }));
        let message = if parsed.is_some() {
            format!("{name} logs at {level}")
        } else {
            format!("{name} logs at its manifest level")
        };
        Ok(Done { message })
    }

    /// Stop every app, in reverse start order.
    pub async fn shutdown(&self) {
        let names: Vec<String> = self.connection_apps().keys().cloned().collect();
        for name in names {
            self.stop_connections(&name);
        }
        self.apps.stop().await;
    }

    /// The admin socket this node's configuration names.
    #[must_use]
    pub fn socket(&self) -> std::path::PathBuf {
        self.config.socket()
    }
}

/// What a bundle's component compiled to.
enum Loaded {
    App(paddock_host::LoadedApp),
    Connection(LoadedConnection),
}

fn listener_info(l: &Listener) -> ListenerInfo {
    let stats = l.stats();
    ListenerInfo {
        name: l.spec().name.clone(),
        address: l.local_addr().to_string(),
        active: stats.active,
        accepted: stats.accepted,
        refused: stats.refused,
        crashed: stats.crashed,
        idle_closed: stats.idle_closed,
    }
}

fn upgrade_response_from(r: &UpgradeReport) -> UpgradeResponse {
    upgrade_response(r)
}
