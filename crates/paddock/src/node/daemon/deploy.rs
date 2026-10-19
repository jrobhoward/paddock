//! Deploying, starting, stopping and removing apps, and building and
//! running a bundle.

use std::path::Path;

use crate::api::{DeployResponse, Done};
use paddock_bundle::manifest::{AppManifest, Durability};
use paddock_bundle::{BundleError, VerifiedBundle};
use paddock_host::{AppDeployment, AppHandle, InstanceSpec, LoadedConnection};
use serde_json::json;

use super::connections::{ConnectionApp, ConnectionBuild};
use super::handlers::{Compiled, HandlerBuild};
use super::{DeployInfo, Node, NodeState};
use crate::node::config::ClusterRole;
use crate::node::error::{NodeError, internal};
use crate::node::gate;
use crate::node::store::AppState;

/// What a bundle runs as.
pub(super) enum Runnable {
    /// Callbacks under a root supervisor.
    App(AppDeployment),
    /// An instance per connection on the app's listeners.
    Connection(ConnectionBuild),
    /// An instance per request on the app's HTTP routes.
    Handler(HandlerBuild),
}

/// Refuse `app` listing a registered name another deployed app lists,
/// running or stopped: names are node-wide, and whichever instance
/// registered first would get the other app's messages (SECURITY.md, S1).
pub(super) fn check_names(st: &NodeState, app: &str, names: &[String]) -> Result<(), NodeError> {
    for (other, info) in st.info.iter().filter(|(other, _)| *other != app) {
        if let Some(name) = names.iter().find(|n| info.names.contains(n)) {
            return Err(NodeError::Conflict(format!(
                "registered name '{name}' is already listed by {other}"
            )));
        }
    }
    Ok(())
}

/// A verified, compiled bundle ready to run.
pub(super) struct Built {
    pub(super) bundle: VerifiedBundle,
    pub(super) run: Runnable,
    pub(super) info: DeployInfo,
}

impl Built {
    fn manifest(&self) -> &AppManifest {
        match &self.run {
            Runnable::App(d) => &d.manifest,
            Runnable::Connection(c) => &c.manifest,
            Runnable::Handler(h) => &h.manifest,
        }
    }

    pub(super) fn name(&self) -> String {
        self.manifest().app.name.clone()
    }

    pub(super) fn version(&self) -> String {
        self.manifest().app.version.to_string()
    }
}

pub(super) fn refused(e: BundleError) -> NodeError {
    NodeError::Refused(e.to_string())
}

/// What a bundle's component compiled to.
// Without the `http` feature no HTTP handler compiles.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub(super) enum Loaded {
    App(paddock_host::LoadedApp),
    Connection(LoadedConnection),
    Handler(Compiled),
}

impl Node {
    /// Start every app recorded as running, in deploy order (which already
    /// respects dependencies).
    pub(super) async fn restore(&self) {
        let _change = self.change.lock().await;
        let order = self.st().desired.order.clone();
        for name in order {
            let Some(app) = self.st().desired.apps.get(&name).cloned() else {
                continue;
            };
            if !app.running {
                continue;
            }
            match self.start_stored(&name, &app).await {
                Ok(()) => self
                    .audit
                    .record("restore", json!({ "app": name, "version": app.current })),
                Err(e) => {
                    tracing::error!(app = %name, error = %e, "cannot restore app");
                    self.audit.record(
                        "restore_failed",
                        json!({ "app": name, "version": app.current, "reason": e.to_string() }),
                    );
                    self.st().restore_errors.insert(name, e.to_string());
                }
            }
        }
    }

    /// Start a deployed app from its stored bundles.
    async fn start_stored(&self, name: &str, app: &AppState) -> Result<(), NodeError> {
        let built = self
            .build(&self.store.bundle_path(name, &app.current))
            .await?;
        built
            .bundle
            .extract_priv(&self.store.priv_dir(name, &app.current))
            .map_err(internal)?;
        let handle = self.run(name, built.run).await?;
        self.st().info.insert(name.to_string(), built.info);
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
                (None, Runnable::Handler(h)) => {
                    if let Some(app) = self.handler_apps().get_mut(name) {
                        app.previous = Some(h);
                    }
                }
                // A previous version of the other kind cannot be rolled back
                // to (the world changed); keep none.
                _ => {}
            }
            self.st().previous_info.insert(name.to_string(), prev.info);
        }
        self.st().restore_errors.remove(name);
        Ok(())
    }

    /// Verify a bundle, check its imports against policy, and compile it.
    pub(super) async fn build(&self, path: &Path) -> Result<Built, NodeError> {
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
        let world = manifest.component.world.clone();
        let cache = self
            .store
            .compiled_path(&name, &version, &manifest.component.checksum);
        let loaded = tokio::task::spawn_blocking(move || match world.as_str() {
            paddock_bundle::CONNECTION_WORLD => rt
                .load_connection_cached(&component, &cache)
                .map(Loaded::Connection)
                .map_err(|e| e.to_string()),
            paddock_bundle::HTTP_HANDLER_WORLD => load_handler(&rt, &component, &cache),
            _ => rt
                .load_cached(&component, &cache)
                .map(Loaded::App)
                .map_err(|e| e.to_string()),
        })
        .await
        .map_err(internal)?
        .map_err(NodeError::Refused)?;
        let mut spec = InstanceSpec::from_manifest(&manifest)
            .with_granted(&grant.granted)
            .with_granted_extensions(&grant.granted_extensions)
            .with_granted_buckets(&grant.granted_buckets)
            .with_granted_secrets(&grant.granted_secrets)
            .with_granted_datasources(&grant.granted_datasources);
        paddock_host::services::egress::within(
            &spec.services.egress.allow,
            self.config.egress.allow.as_deref(),
        )
        .map_err(|e| NodeError::Invalid(format!("wasi:http/outgoing-handler: {e}")))?;
        if let Some(missing) = grant
            .granted_datasources
            .iter()
            .chain(&grant.denied_datasources)
            .find(|d| !self.config.datasources.contains_key(*d))
        {
            return Err(NodeError::Invalid(format!(
                "datasource '{missing}' is not configured on this node ([datasources.{missing}])"
            )));
        }
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
        if !self.config.replicates()
            && let Some((bucket, _)) = spec
                .services
                .keyvalue
                .buckets
                .iter()
                .find(|(_, b)| b.durability == Durability::Replicated)
        {
            return Err(NodeError::Invalid(format!(
                "bucket '{bucket}' is replicated, and this node does not replicate \
                 ([failover] replication = true, with arbiter \"witness\")"
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
            names: manifest
                .spec
                .registered_names
                .iter()
                .map(|n| n.name.clone())
                .collect(),
            signer: manifest.provenance.signer_key.clone(),
            granted: keys(&grant.granted, &grant.granted_extensions)
                .into_iter()
                .chain(grant.granted_buckets.iter().map(|b| format!("bucket:{b}")))
                .chain(grant.granted_secrets.iter().map(|s| format!("secret:{s}")))
                .chain(
                    grant
                        .granted_datasources
                        .iter()
                        .map(|d| format!("datasource:{d}")),
                )
                .collect(),
            denied: keys(&grant.denied, &grant.denied_extensions)
                .into_iter()
                .chain(grant.denied_buckets.iter().map(|b| format!("bucket:{b}")))
                .chain(grant.denied_secrets.iter().map(|s| format!("secret:{s}")))
                .chain(
                    grant
                        .denied_datasources
                        .iter()
                        .map(|d| format!("datasource:{d}")),
                )
                .collect(),
            stubbed: match &loaded {
                Loaded::App(app) => app.stubbed_imports().to_vec(),
                Loaded::Connection(c) => c.stubbed_imports().to_vec(),
                Loaded::Handler(h) => h.stubbed_imports().to_vec(),
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
            Loaded::Handler(handler) => Runnable::Handler(HandlerBuild {
                manifest,
                handler,
                spec,
            }),
        };
        Ok(Built { bundle, run, info })
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
                self.check_slots(name, &deployment)?;
                self.ingress.set_routes(name, &deployment)?;
                let added = match &self.gate {
                    Some(gate) if gate::is_singleton(&deployment) => {
                        gate.hold(name, deployment).await
                    }
                    _ => self.apps.add(deployment).await.map(Some),
                }
                .map_err(|e| NodeError::Invalid(e.to_string()));
                if added.is_err() {
                    self.ingress.remove_routes(name);
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
            Runnable::Handler(build) => {
                self.serve_handler(name, build)?;
                Ok(None)
            }
        }
    }

    /// Refuse `deployment` (as `name`) if the instances that never
    /// passivate, its own with the running apps' and the held singletons',
    /// would need more Store slots than `max_instances`: they would hold
    /// every slot, and an instance needing one would never get it.
    fn check_slots(&self, name: &str, deployment: &AppDeployment) -> Result<(), NodeError> {
        let live = |d: &AppDeployment| {
            let i = &d.manifest.spec.instances;
            if i.passivate_after_ms.is_some() {
                0
            } else {
                u64::from(i.initial())
            }
        };
        // Each app once: a running singleton is in the set and held too.
        let mut by_name: std::collections::BTreeMap<String, u64> = self
            .apps
            .names()
            .iter()
            .filter_map(|n| self.apps.app(n))
            .map(|a| (a.name().to_string(), live(&a.deployment())))
            .collect();
        for d in self.gate.iter().flat_map(|g| g.deployments()) {
            by_name
                .entry(d.manifest.app.name.clone())
                .or_insert_with(|| live(&d));
        }
        by_name.remove(name);
        let others: u64 = by_name.values().sum();
        let wanted = others + live(deployment);
        let slots = u64::from(self.config.max_instances);
        if wanted > slots {
            return Err(NodeError::Invalid(format!(
                "{name}: the instances that never passivate would need {wanted} Store slots, and \
                 this node has {slots} (max_instances); passivate some (instances.passivate_after_ms) \
                 or raise max_instances"
            )));
        }
        Ok(())
    }

    /// Whether `name` runs now, under a supervisor or on listeners.
    fn running(&self, name: &str) -> bool {
        self.apps.app(name).is_some()
            || self.connection_apps().contains_key(name)
            || self.handler_apps().contains_key(name)
            || self.held(name)
    }

    /// Whether `name` is a singleton this node holds, running or not.
    pub(super) fn held(&self, name: &str) -> bool {
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
        if !self.stop_connections(name) && !self.stop_handler(name) && self.apps.app(name).is_some()
        {
            self.apps
                .remove(name)
                .await
                .map_err(|e| NodeError::Conflict(e.to_string()))?;
        }
        Ok(())
    }

    /// Deploy a new app from bundle bytes.
    ///
    /// # Errors
    ///
    /// [`NodeError::Refused`] for an untrusted, corrupt or non-conforming
    /// bundle; [`NodeError::Conflict`] if the app is already deployed;
    /// [`NodeError::Invalid`] for a missing dependency or volume.
    pub async fn deploy(&self, bytes: &[u8]) -> Result<DeployResponse, NodeError> {
        let _change = self.change.lock().await;
        let upload = self.store.write_upload(bytes)?;
        let result = self.deploy_upload(&upload).await;
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

    async fn deploy_upload(&self, upload: &Path) -> Result<DeployResponse, NodeError> {
        if self.config.cluster.role == ClusterRole::Witness {
            return Err(NodeError::Refused(
                "this node is a witness, which runs no apps".into(),
            ));
        }
        let built = self.build(upload).await?;
        let (name, version) = (built.name(), built.version());
        if self.st().desired.apps.contains_key(&name) {
            return Err(NodeError::Conflict(format!(
                "{name} is already deployed; upgrade it instead"
            )));
        }
        check_names(&self.st(), &name, &built.info.names)?;
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
        {
            let mut st = self.st();
            st.desired.order.push(name.clone());
            st.desired.apps.insert(name.clone(), app.clone());
        }
        self.save_desired()?;
        self.st().info.insert(name.clone(), info.clone());
        Ok(DeployResponse {
            app: self.summary(&name, &app, None),
            granted: info.granted,
            denied: info.denied,
            warnings: info.warnings,
        })
    }

    /// Stop an app; it stays deployed and does not start on restart.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`], or [`NodeError::Conflict`] if it is already
    /// stopped or a running app depends on it.
    pub async fn stop(&self, name: &str) -> Result<Done, NodeError> {
        let _change = self.change.lock().await;
        let running = self
            .st()
            .desired
            .apps
            .get(name)
            .ok_or_else(|| NodeError::NotFound(format!("{name} is not deployed")))?
            .running;
        if !running {
            return Err(NodeError::Conflict(format!("{name} is already stopped")));
        }
        self.stop_running(name).await?;
        self.ingress.remove_routes(name);
        {
            let mut st = self.st();
            if let Some(app) = st.desired.apps.get_mut(name) {
                app.running = false;
            }
            st.restore_errors.remove(name);
        }
        self.save_desired()?;
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
        let _change = self.change.lock().await;
        let app = self
            .st()
            .desired
            .apps
            .get(name)
            .cloned()
            .ok_or_else(|| NodeError::NotFound(format!("{name} is not deployed")))?;
        if self.running(name) {
            return Err(NodeError::Conflict(format!("{name} is already running")));
        }
        let started = self.start_stored(name, &app).await;
        if let Err(e) = &started {
            self.audit.record(
                "start_refused",
                json!({ "app": name, "reason": e.to_string() }),
            );
        }
        started?;
        if let Some(entry) = self.st().desired.apps.get_mut(name) {
            entry.running = true;
        }
        self.save_desired()?;
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
        let _change = self.change.lock().await;
        if !self.st().desired.apps.contains_key(name) {
            return Err(NodeError::NotFound(format!("{name} is not deployed")));
        }
        self.stop_running(name).await?;
        self.ingress.remove_routes(name);
        self.store.remove_app(name);
        {
            let mut st = self.st();
            st.desired.apps.remove(name);
            st.desired.order.retain(|n| n != name);
            st.info.remove(name);
            st.previous_info.remove(name);
            st.restore_errors.remove(name);
        }
        self.save_desired()?;
        self.audit.record("remove", json!({ "app": name }));
        Ok(Done {
            message: format!("{name} removed"),
        })
    }
}

/// Compile an HTTP handler; refused in a build without the HTTP ingress.
#[cfg(feature = "http")]
fn load_handler(
    rt: &paddock_host::Runtime,
    component: &[u8],
    cache: &Path,
) -> Result<Loaded, String> {
    rt.load_handler(component, Some(cache))
        .map(Loaded::Handler)
        .map_err(|e| e.to_string())
}

/// Compile an HTTP handler; refused in a build without the HTTP ingress.
#[cfg(not(feature = "http"))]
fn load_handler(
    _rt: &paddock_host::Runtime,
    _component: &[u8],
    _cache: &Path,
) -> Result<Loaded, String> {
    Err("an HTTP handler needs the HTTP ingress, and this build of paddock has none (cargo feature `http`)".into())
}
