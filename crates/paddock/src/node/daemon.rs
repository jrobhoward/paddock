//! The node: a runtime, its apps, the deploy store and the operations the
//! admin API exposes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::api::{
    AppDetail, AppSummary, DeployResponse, Done, FailoverInfo, InstanceInfo, ListenerInfo, LogLine,
    MemberInfo, RolloutParams, StateResponse, Target, UpgradeResponse,
};
use paddock_bundle::manifest::{AppManifest, Capability, UpgradeMode};
use paddock_bundle::{BundleError, OperatorPolicy, VerifiedBundle};
use paddock_host::failover::{Arbiter, FailoverConfig, FailoverHandle, WitnessConfig};
use paddock_host::http::{HttpIngress, HttpRoute, IngressListen, IngressTls, TlsCertificate};
use paddock_host::services::{Extension, StartedExtensions};
use paddock_host::transport::quic::{NodeIdentity, QuicConfig, QuicTransport};
use paddock_host::{
    AppDeployment, AppHandle, AppStatus, Apps, CallError, Envelope, HostObjects, InstanceHandle,
    InstanceSpec, Listener, ListenerSpec, LoadedConnection, MemoryProfile as HostProfile, Pid,
    Role, Runtime, RuntimeConfig, SecretSource, SharedBucket, UpgradeError, UpgradeOptions,
    UpgradeReport,
};
use serde_json::json;
use tokio::sync::Mutex;

use super::audit::Audit;
use super::config::{ArbiterKind, ClusterRole, MemoryProfile, NodeConfig};
use super::error::{NodeError, internal};
use super::gate::{self, Gate};
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
    /// Extensions' node-level work, stopped on shutdown.
    extensions: std::sync::Mutex<Option<StartedExtensions>>,
    /// HTTP ingress, if node config has `[http]` addresses.
    http: Option<HttpIngress>,
    /// Stops background tasks: the `[metrics]` endpoint, certificate
    /// checks.
    stop: tokio::sync::watch::Sender<bool>,
    /// HTTPS certificates, by ingress name.
    certificates: Vec<(String, IngressTls)>,
    /// The cluster transport, with `cluster.address`.
    quic: std::sync::Mutex<Option<QuicTransport>>,
    /// The singletons, with `[failover]`.
    gate: Option<Arc<Gate>>,
    /// This node's failover, once started.
    failover: std::sync::OnceLock<FailoverHandle>,
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
    /// A pid on another node, reached through the cluster.
    Remote(Pid),
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
        pid: pid.text(),
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
                address: config
                    .cluster
                    .advertise
                    .clone()
                    .or_else(|| config.cluster.address.clone())
                    .unwrap_or_default(),
                host_objects: HostObjects {
                    timers: config.limits.timers_max,
                    monitors: config.limits.monitors_max,
                    links: config.limits.links_max,
                    pending_calls: config.limits.pending_calls_max,
                    groups: config.limits.groups_max,
                    watches: config.limits.watches_max,
                },
                secrets: config
                    .secrets
                    .iter()
                    .filter_map(|(name, s)| {
                        let source = match (&s.file, &s.env) {
                            (Some(f), _) => SecretSource::File(f.clone()),
                            (None, Some(v)) => SecretSource::Env(v.clone()),
                            (None, None) => return None,
                        };
                        Some((name.clone(), source))
                    })
                    .collect(),
                buckets: config
                    .buckets
                    .iter()
                    .map(|(name, b)| {
                        let bucket = SharedBucket {
                            durability: b.durability,
                            quota: b.quota.map(|q| q.bytes()),
                        };
                        (name.clone(), bucket)
                    })
                    .collect(),
            },
            extensions,
        )
        .map_err(internal)?;
        let apps = rt.start_apps(Vec::new()).await.map_err(internal)?;
        let mut certificates = Vec::new();
        let http = if config.http.is_empty() {
            None
        } else {
            let ingress = HttpIngress::new(apps.clone());
            for (name, binding) in &config.http {
                let tls = if binding.tls.is_empty() {
                    None
                } else {
                    let entries = binding
                        .tls
                        .iter()
                        .map(|t| TlsCertificate {
                            cert: t.cert.clone(),
                            key: t.key.clone(),
                            hosts: t.hosts.clone(),
                        })
                        .collect();
                    let tls = IngressTls::load(entries)
                        .map_err(|e| NodeError::Invalid(format!("http.{name}: {e}")))?;
                    certificates.push((name.clone(), tls.clone(), binding.tls_reload_s));
                    Some(tls)
                };
                let bound = ingress
                    .listen(IngressListen {
                        address: binding.address,
                        acceptors: binding.acceptors,
                        max_connections: binding.max_connections,
                        header_timeout: Duration::from_millis(binding.header_timeout_ms.max(1)),
                        body_timeout: Duration::from_millis(binding.body_timeout_ms.max(1)),
                        max_response: binding.max_response.bytes(),
                        tls,
                    })
                    .await
                    .map_err(|e| NodeError::Invalid(format!("http.{name}: {e}")))?;
                tracing::info!(name, %bound, "HTTP ingress listening");
            }
            Some(ingress)
        };
        let audit = Audit::new(store.audit_path());
        let gate = config
            .failover
            .is_some()
            .then(|| Arc::new(Gate::new(apps.clone())));
        let node = Self {
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
            extensions: std::sync::Mutex::default(),
            http,
            stop: tokio::sync::watch::channel(false).0,
            certificates: certificates
                .iter()
                .map(|(n, t, _)| (n.clone(), t.clone()))
                .collect(),
            quic: std::sync::Mutex::default(),
            gate,
            failover: std::sync::OnceLock::new(),
            config,
        };
        for (name, tls, every) in certificates {
            if every > 0 {
                tokio::spawn(watch_certificates(
                    name,
                    tls,
                    Duration::from_secs(every),
                    node.stop.subscribe(),
                ));
            }
        }
        if let Some(binding) = &node.config.metrics {
            let listener = tokio::net::TcpListener::bind(binding.address)
                .await
                .map_err(|e| NodeError::Invalid(format!("metrics: {}: {e}", binding.address)))?;
            let (rt, http) = (node.rt.clone(), node.http.clone());
            let render = move || metrics_text(&rt, http.as_ref());
            tokio::spawn(super::server::serve_metrics(
                listener,
                render,
                node.stop.subscribe(),
            ));
        }
        node.restore().await;
        let started = match node
            .rt
            .start_extensions(&node.apps, &node.config.extensions)
        {
            Ok(started) => started,
            Err(e) => {
                node.shutdown().await;
                return Err(NodeError::Invalid(e.to_string()));
            }
        };
        *node
            .extensions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(started);
        if let Err(e) = node.start_cluster().and_then(|()| node.start_failover()) {
            node.shutdown().await;
            return Err(e);
        }
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
                (None, Runnable::App(d)) => {
                    if let Some(gate) = &self.gate {
                        gate.set_previous(name, d);
                    }
                }
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
            .with_granted_extensions(&grant.granted_extensions)
            .with_granted_buckets(&grant.granted_buckets)
            .with_granted_secrets(&grant.granted_secrets);
        if let Some(missing) = grant
            .granted_secrets
            .iter()
            .chain(&grant.denied_secrets)
            .find(|s| !self.config.secrets.contains_key(*s))
        {
            return Err(NodeError::Invalid(format!(
                "secret '{missing}' is not configured on this node"
            )));
        }
        if let Some(missing) = spec
            .services
            .keyvalue
            .shared
            .keys()
            .chain(&grant.denied_buckets)
            .find(|b| !self.config.buckets.contains_key(*b))
        {
            return Err(NodeError::Invalid(format!(
                "shared bucket '{missing}' is not configured on this node"
            )));
        }
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
            granted: keys(&grant.granted, &grant.granted_extensions)
                .into_iter()
                .chain(grant.granted_buckets.iter().map(|b| format!("bucket:{b}")))
                .chain(grant.granted_secrets.iter().map(|s| format!("secret:{s}")))
                .collect(),
            denied: keys(&grant.denied, &grant.denied_extensions)
                .into_iter()
                .chain(grant.denied_buckets.iter().map(|b| format!("bucket:{b}")))
                .chain(grant.denied_secrets.iter().map(|s| format!("secret:{s}")))
                .collect(),
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
            Runnable::App(deployment) => {
                if let Some(gate) = &self.gate
                    && !gate::is_singleton(&deployment)
                    && let Some(dep) = deployment
                        .manifest
                        .spec
                        .dependencies
                        .iter()
                        .find(|d| gate.holds(&d.app))
                {
                    return Err(NodeError::Invalid(format!(
                        "{name} depends on {}, a singleton, which runs on one node of the pair only; \
                         make {name} a singleton too",
                        dep.app
                    )));
                }
                self.set_routes(name, &deployment)?;
                let added = match &self.gate {
                    Some(gate) if gate::is_singleton(&deployment) => {
                        gate.hold(name, deployment).await
                    }
                    _ => self.apps.add(deployment).await.map(Some),
                }
                .map_err(|e| NodeError::Invalid(e.to_string()));
                if added.is_err() {
                    self.remove_routes(name);
                }
                added
            }
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

    /// Give `name` the HTTP routes `deployment` declares, if the operator
    /// granted them (none otherwise).
    fn set_routes(&self, name: &str, deployment: &AppDeployment) -> Result<(), NodeError> {
        let routes = if deployment.spec.granted.contains(&Capability::HttpIncoming) {
            HttpRoute::from_manifest(&deployment.manifest)
        } else {
            Vec::new()
        };
        match &self.http {
            Some(ingress) => ingress
                .set_routes(name, routes)
                .map_err(|e| NodeError::Conflict(e.to_string())),
            None if routes.is_empty() => Ok(()),
            None => Err(NodeError::Invalid(format!(
                "{name} declares HTTP routes, and this node has no HTTP ingress ([http.<name>] in node config)"
            ))),
        }
    }

    fn remove_routes(&self, name: &str) {
        if let Some(ingress) = &self.http {
            ingress.remove_routes(name);
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
        self.apps.app(name).is_some()
            || self.connection_apps().contains_key(name)
            || self.held(name)
    }

    /// Whether `name` is a singleton this node holds, running or not.
    fn held(&self, name: &str) -> bool {
        self.gate.as_ref().is_some_and(|g| g.holds(name))
    }

    /// Stop running `name`, if it runs: a singleton, a supervised app or a
    /// connection handler.
    async fn stop_running(&self, name: &str) -> Result<(), NodeError> {
        if let Some(gate) = &self.gate
            && gate
                .release(name)
                .await
                .map_err(|e| NodeError::Conflict(e.to_string()))?
        {
            return Ok(());
        }
        if !self.stop_connections(name) && self.apps.app(name).is_some() {
            self.apps
                .remove(name)
                .await
                .map_err(|e| NodeError::Conflict(e.to_string()))?;
        }
        Ok(())
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
            (None, None, _) if self.held(name) => (
                "standby".into(),
                Some("a singleton; runs while this node is primary".into()),
            ),
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
            Runnable::App(deployment) if self.held(name) && self.apps.app(name).is_none() => {
                self.upgrade_held(name, deployment, params)
            }
            Runnable::App(deployment) => {
                let snapshot = deployment.manifest.spec.upgrade.mode == UpgradeMode::Snapshot;
                let before = self.http.as_ref().map(|h| h.routes(name));
                match self.set_routes(name, &deployment) {
                    Err(e) => Err((e, false)),
                    Ok(()) => {
                        let upgraded = self
                            .apps
                            .upgrade(name, deployment, &options(params))
                            .await
                            .map(|r| upgrade_response(&r))
                            .map_err(|e| (rollout_error(e, snapshot, params), true));
                        if upgraded.is_err()
                            && let (Some(http), Some(before)) = (&self.http, before)
                        {
                            // The old version keeps running with its routes.
                            let _ = http.set_routes(name, before);
                        }
                        if let Some(gate) = &self.gate {
                            gate.sync(name);
                        }
                        upgraded
                    }
                }
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

    /// Upgrade a singleton this node holds but does not run: a promotion
    /// starts the new version.
    fn upgrade_held(
        &self,
        name: &str,
        deployment: AppDeployment,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, (NodeError, bool)> {
        let gate = self
            .gate
            .as_ref()
            .ok_or_else(|| (internal("no failover"), false))?;
        let to = deployment.manifest.app.version.to_string();
        let (from, approved) = gate
            .upgrade_held(name, deployment, params.approve)
            .map_err(|e| (upgrade_error(e), false))?;
        Ok(UpgradeResponse {
            from,
            to,
            mode: "held".into(),
            instances: Vec::new(),
            pauses_us: Vec::new(),
            approved,
        })
    }

    /// Roll back a singleton this node holds but does not run.
    fn rollback_held(&self, name: &str) -> Result<UpgradeResponse, NodeError> {
        let gate = self.gate.as_ref().ok_or_else(|| internal("no failover"))?;
        let (from, to) = gate.rollback_held(name).map_err(upgrade_error)?;
        Ok(UpgradeResponse {
            from,
            to,
            mode: "held".into(),
            instances: Vec::new(),
            pauses_us: Vec::new(),
            approved: Vec::new(),
        })
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
            None if self.held(name) && self.apps.app(name).is_none() => self.rollback_held(name),
            None => {
                let snapshot = self.apps.app(name).and_then(|a| a.rollback_mode())
                    == Some(UpgradeMode::Snapshot);
                let rolled = self
                    .apps
                    .rollback(name, &options(params))
                    .await
                    .map(|r| upgrade_response_from(&r))
                    .map_err(|e| rollout_error(e, snapshot, params));
                if rolled.is_ok()
                    && let Some(app) = self.apps.app(name)
                    && let Err(e) = self.set_routes(name, &app.deployment())
                {
                    tracing::warn!(app = name, error = %e, "routes after rollback");
                }
                if let Some(gate) = &self.gate {
                    gate.sync(name);
                }
                rolled
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
        self.stop_running(name).await?;
        self.remove_routes(name);
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
        self.stop_running(name).await?;
        self.remove_routes(name);
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
            Resolved::Remote(pid) => self.rt.call(pid, msg, timeout).await,
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
            Resolved::Remote(pid) => self.rt.cast(pid, msg),
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
                if pid.node != self.rt.node() {
                    return Ok(Resolved::Remote(pid));
                }
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
        let shown = pid.text();
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
        let started = self
            .extensions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(started);
        self.stop.send_replace(true);
        if let Some(gate) = &self.gate {
            gate.close().await;
        }
        let quic = self
            .quic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(quic) = quic {
            self.rt.leave_cluster();
            quic.shutdown_gracefully().await;
        }
        if let Some(http) = &self.http {
            http.stop();
        }
        let names: Vec<String> = self.connection_apps().keys().cloned().collect();
        for name in names {
            self.stop_connections(&name);
        }
        self.apps.stop().await;
    }

    /// Every app's metrics and the node's, in the Prometheus text format:
    /// the runtime's, then the HTTP ingress's.
    #[must_use]
    pub fn metrics(&self) -> String {
        metrics_text(&self.rt, self.http.as_ref())
    }

    /// The cluster's members as this node knows them, itself included.
    #[must_use]
    pub fn members(&self) -> Vec<MemberInfo> {
        let me = self.rt.node();
        self.rt
            .members()
            .into_iter()
            .map(|m| MemberInfo {
                reachable: m.node == me || self.rt.peer_status(m.node).is_some_and(|s| s.reachable),
                node: m.node,
                creation: m.creation,
                address: m.address,
                role: match m.role {
                    Role::Witness => "witness".into(),
                    _ => "member".into(),
                },
            })
            .collect()
    }

    /// Start the QUIC transport on `cluster.address`, join the seeds, and
    /// watch the identity files.
    fn start_cluster(&self) -> Result<(), NodeError> {
        let cluster = &self.config.cluster;
        let (Some(address), Some(key), Some(peers)) =
            (&cluster.address, &cluster.key, &cluster.peers)
        else {
            return Ok(());
        };
        let bind = address
            .parse()
            .map_err(|e| NodeError::Invalid(format!("cluster.address: {e}")))?;
        let quic = QuicTransport::start(
            &self.rt,
            QuicConfig {
                bind,
                identity: NodeIdentity {
                    key: key.clone(),
                    peers: peers.clone(),
                },
                idle_timeout: Duration::from_millis(cluster.node_timeout_ms.max(1) * 2),
            },
        )
        .map_err(|e| NodeError::Invalid(e.to_string()))?;
        tracing::info!(%bind, "cluster transport listening");
        for seed in &cluster.seeds {
            let Ok(seed) = seed.parse() else {
                continue;
            };
            let quic = quic.clone();
            tokio::spawn(async move {
                match quic.join_seed(seed).await {
                    Ok(node) => tracing::info!(%seed, node, "joined through seed"),
                    Err(e) => tracing::warn!(error = %e, "seed not reached yet; retrying"),
                }
            });
        }
        if cluster.identity_reload_s > 0 {
            let (quic, every) = (quic.clone(), Duration::from_secs(cluster.identity_reload_s));
            let mut stop = self.stop.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.wait_for(|s| *s) => return,
                        () = tokio::time::sleep(every) => {}
                    }
                    match quic.reload_if_changed() {
                        Ok(true) => tracing::info!("cluster key or peers reloaded"),
                        Ok(false) => {}
                        Err(e) => {
                            tracing::error!(error = %e, "cluster identity reload failed; keeping the old")
                        }
                    }
                }
            });
        }
        *self
            .quic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(quic);
        Ok(())
    }

    /// Start `[failover]` at the epoch kept on disk and follow its
    /// leadership, or serve `[witness]` from its kept state.
    fn start_failover(&self) -> Result<(), NodeError> {
        if let Some(w) = &self.config.witness {
            let mut cfg = WitnessConfig::new(w.pair, w.first);
            cfg.lease = Duration::from_millis(w.lease_ms);
            cfg.margin = Duration::from_millis(w.margin_ms);
            cfg.resume = self.store.load_witness()?;
            self.rt
                .serve_witness_kept(cfg, Some(self.store.witness_persist()))
                .map_err(|e| NodeError::Invalid(format!("witness: {e}")))?;
            tracing::info!(pair = ?w.pair, "serving as witness");
        }
        let (Some(f), Some(gate)) = (&self.config.failover, &self.gate) else {
            return Ok(());
        };
        let arbiter = match (f.arbiter, f.witness) {
            (ArbiterKind::Witness, Some(w)) => Arbiter::Witness(w),
            (ArbiterKind::Operator, _) => Arbiter::Operator,
            _ => Arbiter::Paths,
        };
        let epoch = self.store.load_epoch()?;
        let mut cfg = FailoverConfig::new(f.partner, arbiter);
        cfg.lease = Duration::from_millis(f.lease_ms);
        cfg.margin = Duration::from_millis(f.margin_ms);
        cfg.epoch = epoch;
        let handle = self
            .rt
            .start_failover(cfg)
            .map_err(|e| NodeError::Invalid(format!("failover: {e}")))?;
        tracing::info!(partner = f.partner, epoch, "failover started");
        tokio::spawn(gate::follow(
            gate.clone(),
            self.rt.clone(),
            self.store.epoch_saver(),
            epoch,
            handle.subscribe(),
            self.stop.subscribe(),
        ));
        let _ = self.failover.set(handle);
        Ok(())
    }

    /// This node's failover standing.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`.
    pub fn failover(&self) -> Result<FailoverInfo, NodeError> {
        let (Some(f), Some(handle)) = (&self.config.failover, self.failover.get()) else {
            return Err(NodeError::NotFound(
                "this node has no [failover] in its config".into(),
            ));
        };
        let now = handle.leadership();
        let singletons = self.gate.as_ref().map(|g| g.names()).unwrap_or_default();
        Ok(FailoverInfo {
            partner: f.partner,
            arbiter: match f.arbiter {
                ArbiterKind::Witness => format!("witness {}", f.witness.unwrap_or_default()),
                ArbiterKind::Paths => "paths".into(),
                ArbiterKind::Operator => "operator".into(),
            },
            role: gate::role_name(now.standing).into(),
            epoch: now.epoch,
            may_run: now.may_run,
            awaiting_confirmation: now.awaiting_confirmation,
            partner_reachable: self.rt.peer_status(f.partner).is_some_and(|s| s.reachable),
            singletons,
        })
    }

    /// An operator confirms that the partner is gone, so the standby may
    /// promote itself (`paddock_host::failover::FailoverHandle::confirm`).
    /// Safe only if the partner is down or cut off: with
    /// `arbiter = "operator"`, or a witness that cannot be reached.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`.
    pub fn confirm_failover(&self) -> Result<Done, NodeError> {
        let handle = self.failover.get().ok_or_else(|| {
            NodeError::NotFound("this node has no [failover] in its config".into())
        })?;
        let now = handle.leadership();
        handle.confirm();
        self.audit.record(
            "failover_confirm",
            json!({ "epoch": now.epoch, "awaiting": now.awaiting_confirmation }),
        );
        Ok(Done {
            message: if now.awaiting_confirmation {
                "confirmed: this node is promoted once the partner's lease has run out".into()
            } else {
                "confirmed; this node was not waiting for confirmation, so nothing may change"
                    .into()
            },
        })
    }

    /// Reload every HTTPS ingress's certificates from their files. An
    /// ingress whose files fail keeps its old certificates.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] naming each ingress that failed.
    pub fn reload_tls(&self) -> Result<Done, NodeError> {
        let mut failed: Vec<String> = self
            .certificates
            .iter()
            .filter_map(|(name, tls)| tls.reload().err().map(|e| format!("http.{name}: {e}")))
            .collect();
        let quic = self
            .quic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(quic) = quic
            && let Err(e) = quic.reload()
        {
            failed.push(format!("cluster: {e}"));
        }
        self.audit.record(
            "reload_tls",
            json!({ "ingresses": self.certificates.len(), "failed": failed }),
        );
        if !failed.is_empty() {
            return Err(NodeError::Invalid(failed.join("; ")));
        }
        Ok(Done {
            message: format!("reloaded {} HTTPS ingress(es)", self.certificates.len()),
        })
    }

    /// The admin socket this node's configuration names.
    #[must_use]
    pub fn socket(&self) -> std::path::PathBuf {
        self.config.socket()
    }
}

/// Reload `tls` whenever its files change, checking every `every`, until
/// `stop`.
async fn watch_certificates(
    name: String,
    tls: IngressTls,
    every: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            () = tokio::time::sleep(every) => {}
        }
        match tls.reload_if_changed() {
            Ok(true) => tracing::info!(ingress = name, "TLS certificates reloaded"),
            Ok(false) => {}
            Err(e) => {
                tracing::error!(ingress = name, error = %e, "TLS reload failed; keeping the old certificates")
            }
        }
    }
}

/// The runtime's metrics, then the HTTP ingress's.
fn metrics_text(rt: &Runtime, http: Option<&HttpIngress>) -> String {
    let mut text = rt.render_metrics();
    if let Some(http) = http {
        let s = http.stats();
        for (name, help, value) in [
            (
                "paddock_http_requests_total",
                "HTTP requests answered.",
                s.requests,
            ),
            (
                "paddock_http_server_errors_total",
                "HTTP requests answered with a 5xx status.",
                s.server_errors,
            ),
            (
                "paddock_http_not_found_total",
                "HTTP requests that matched no route.",
                s.not_found,
            ),
            (
                "paddock_http_refused_connections_total",
                "Connections closed on accept, over max_connections.",
                s.refused,
            ),
        ] {
            text.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
            ));
        }
    }
    text
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
