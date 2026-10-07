//! The node: a runtime, its apps, the deploy store and the operations the
//! admin API exposes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::api::{
    AppDetail, AppSummary, DeployResponse, Done, InstanceInfo, LogLine, RolloutParams,
    StateResponse, UpgradeResponse,
};
use paddock_bundle::manifest::UpgradeMode;
use paddock_bundle::{BundleError, OperatorPolicy, VerifiedBundle};
use paddock_host::{
    AppDeployment, AppHandle, AppStatus, Apps, CallError, Envelope, InstanceHandle, InstanceSpec,
    MemoryProfile as HostProfile, Pid, Runtime, RuntimeConfig, UpgradeError, UpgradeOptions,
    UpgradeReport,
};
use serde_json::json;
use tokio::sync::Mutex;

use super::audit::Audit;
use super::config::{MemoryProfile, NodeConfig};
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
    deployment: AppDeployment,
    info: DeployInfo,
}

impl Built {
    fn name(&self) -> String {
        self.deployment.manifest.app.name.clone()
    }

    fn version(&self) -> String {
        self.deployment.manifest.app.version.to_string()
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
        let rt = Runtime::new(RuntimeConfig {
            node: config.node,
            max_instances: config.max_instances,
            memory_profile: match config.memory_profile {
                MemoryProfile::Standard => HostProfile::Standard,
                MemoryProfile::Dense => HostProfile::Dense,
            },
            epoch_tick: Duration::from_millis(config.epoch_tick_ms.max(1)),
            data_dir: Some(store.kv_dir()),
        })
        .map_err(internal)?;
        let apps = rt.start_apps(Vec::new()).await.map_err(internal)?;
        let audit = Audit::new(store.audit_path());
        let desired = store.load_state()?;
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
        let handle = self
            .apps
            .add(built.deployment)
            .await
            .map_err(|e| NodeError::Invalid(e.to_string()))?;
        st.info.insert(name.to_string(), built.info);
        if let Some(previous) = &app.previous {
            let prev = self.build(&self.store.bundle_path(name, previous)).await?;
            handle.set_previous(Some(prev.deployment));
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
        let app = tokio::task::spawn_blocking(move || rt.load(&component))
            .await
            .map_err(internal)?
            .map_err(|e| NodeError::Refused(e.to_string()))?;
        let mut spec = InstanceSpec::from_manifest(&manifest).with_granted(&grant.granted);
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
        let keys = |set: &std::collections::BTreeSet<paddock_bundle::manifest::Capability>| {
            set.iter().map(|c| c.key().to_string()).collect::<Vec<_>>()
        };
        let info = DeployInfo {
            signer: manifest.provenance.signer_key.clone(),
            granted: keys(&grant.granted),
            denied: keys(&grant.denied),
            stubbed: app.stubbed_imports().to_vec(),
            warnings: manifest
                .spec
                .unenforced_fields()
                .iter()
                .map(ToString::to_string)
                .collect(),
        };
        Ok(Built {
            bundle,
            deployment: AppDeployment {
                manifest,
                app,
                spec,
            },
            info,
        })
    }

    fn summary(&self, name: &str, app: &AppState, restore_error: Option<&String>) -> AppSummary {
        let handle = self.apps.app(name);
        let (status, reason) = match (&handle, restore_error) {
            (Some(h), _) => status_text(&h.status()),
            (None, Some(e)) => ("failed".into(), Some(e.clone())),
            (None, None) => ("stopped".into(), None),
        };
        AppSummary {
            name: name.to_string(),
            version: app.current.clone(),
            status,
            reason,
            instances: handle.map_or(0, |h| h.instances().len()),
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
        if let Err(e) = self.apps.add(built.deployment).await {
            self.store.remove_version(&name, &version);
            return Err(NodeError::Invalid(e.to_string()));
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
        let snapshot = built.deployment.manifest.spec.upgrade.mode == UpgradeMode::Snapshot;
        match self
            .apps
            .upgrade(name, built.deployment, &options(params))
            .await
        {
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
                Ok(upgrade_response(&report))
            }
            Err(e) => {
                if current.previous.as_deref() != Some(version.as_str()) {
                    self.store.remove_version(name, &version);
                }
                Err(rollout_error(e, snapshot, params))
            }
        }
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
        let snapshot =
            self.apps.app(name).and_then(|a| a.rollback_mode()) == Some(UpgradeMode::Snapshot);
        let result = self
            .apps
            .rollback(name, &options(params))
            .await
            .map_err(|e| rollout_error(e, snapshot, params));
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
        result.map(|r| upgrade_response_from(&r))
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
        if self.apps.app(name).is_some() {
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
        if self.apps.app(name).is_some() {
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
        if self.apps.app(name).is_some() {
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
        let app = self
            .apps
            .app(name)
            .ok_or_else(|| NodeError::NotFound(format!("{name} is not running")))?;
        app.call(msg, timeout).await.map_err(|e| match e {
            CallError::Noproc => NodeError::NotFound(format!("{name} has no running instance")),
            CallError::TooLarge => NodeError::Invalid(e.to_string()),
            other => NodeError::Conflict(format!("call to {name} failed: {other}")),
        })
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
        let app = self
            .apps
            .app(name)
            .ok_or_else(|| NodeError::NotFound(format!("{name} is not running")))?;
        app.cast(msg).map_err(|e| match e {
            CallError::Noproc => NodeError::NotFound(format!("{name} has no running instance")),
            CallError::TooLarge => NodeError::Invalid(e.to_string()),
            other => NodeError::Conflict(format!("cast to {name} failed: {other}")),
        })?;
        Ok(Done {
            message: format!("cast to {name}"),
        })
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
        self.apps.stop().await;
    }

    /// The admin socket this node's configuration names.
    #[must_use]
    pub fn socket(&self) -> std::path::PathBuf {
        self.config.socket()
    }
}

fn upgrade_response_from(r: &UpgradeReport) -> UpgradeResponse {
    upgrade_response(r)
}
