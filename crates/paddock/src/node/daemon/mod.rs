//! The node: a runtime, its apps, the deploy store and the operations the
//! admin API exposes.

mod blue_green;
mod cluster;
mod connections;
mod deploy;
mod handlers;
mod ingress;
mod queries;
#[cfg(feature = "tls")]
mod remote;
mod upgrade;

pub use blue_green::Traffic;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::api::Done;
use paddock_bundle::OperatorPolicy;
use paddock_bundle::manifest::{ByteSize, Durability};
use paddock_host::failover::FailoverHandle;
use paddock_host::services::{Extension, StartedExtensions};
use paddock_host::transport::quic::QuicTransport;
use paddock_host::{
    Apps, HostObjects, MemoryProfile as HostProfile, Role, Runtime, RuntimeConfig, SecretSource,
    SharedBucket,
};
use serde_json::json;

use super::audit::Audit;
use super::config::{BucketScope, ClusterRole, MemoryProfile, NodeConfig};
use super::error::{NodeError, internal};
use super::events::{self, Events};
use super::gate::Gate;
use super::logring::LogRing;
use super::store::{State, Store};

use connections::ConnectionApp;
use handlers::HandlerApp;
use ingress::Ingress;

/// A running Paddock node.
#[derive(Debug)]
pub struct Node {
    config: NodeConfig,
    rt: Runtime,
    apps: Apps,
    store: Store,
    audit: Audit,
    /// The event stream (`GET /v1/events`), which the audit log feeds too.
    events: Events,
    logs: Arc<LogRing>,
    policy: OperatorPolicy,
    /// Held for the whole of a change (deploy, upgrade, rollback, start,
    /// stop, remove, restore), so changes run one at a time.
    change: tokio::sync::Mutex<()>,
    /// What is deployed and what each deploy decided. Locked only briefly,
    /// never across an await, so reads (`list`, `status`) never wait behind
    /// a change; a change commits to it when it is done.
    state: std::sync::Mutex<NodeState>,
    /// Running connection-handler apps, by name. Locked only briefly,
    /// never across an await.
    connections: std::sync::Mutex<HashMap<String, ConnectionApp>>,
    /// HTTP-handler apps (`wasi:http/proxy`), served by the ingress.
    handlers: std::sync::Mutex<HashMap<String, HandlerApp>>,
    /// Extensions' node-level work, stopped on shutdown.
    extensions: std::sync::Mutex<Option<StartedExtensions>>,
    /// HTTP ingress, if node config has `[http]` addresses.
    ingress: Ingress,
    /// Stops background tasks: the `[metrics]` endpoint, certificate
    /// checks.
    stop: tokio::sync::watch::Sender<bool>,
    /// The cluster transport, with `cluster.address`.
    quic: Arc<std::sync::Mutex<Option<QuicTransport>>>,
    /// Who may use the remote admin API (`[admin] operators`).
    #[cfg(feature = "tls")]
    operators: remote::Operators,
    /// The singletons, with `[failover]`.
    gate: Option<Arc<Gate>>,
    /// This node's failover, once started.
    failover: std::sync::OnceLock<FailoverHandle>,
    /// Raised by every change of the failover's leadership, for
    /// `GET /v1/failover?after=`.
    leadership: tokio::sync::watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct NodeState {
    desired: State,
    info: HashMap<String, DeployInfo>,
    previous_info: HashMap<String, DeployInfo>,
    /// Blue-green upgrades' new versions, until promoted or dropped.
    new_info: HashMap<String, DeployInfo>,
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
    /// The registered names the manifest lists, reserved for this app.
    names: Vec<String>,
}

impl Node {
    fn st(&self) -> std::sync::MutexGuard<'_, NodeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write the desired state to the store. Called under the change lock,
    /// so no other change moves it meanwhile.
    fn save_desired(&self) -> Result<(), NodeError> {
        let desired = self.st().desired.clone();
        self.store.save_state(&desired)
    }

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
        #[cfg(not(feature = "sql"))]
        if let Some(name) = config.datasources.keys().next() {
            return Err(NodeError::Invalid(format!(
                "datasources.{name}: this build of paddock has no SQL (cargo feature `sql`)"
            )));
        }
        let rt = Runtime::with_extensions(
            RuntimeConfig {
                node: config.node,
                max_instances: config.max_instances,
                max_processes: config.max_processes,
                max_core_instances_per_component: config.max_core_instances_per_component,
                memory_profile: match config.memory_profile {
                    MemoryProfile::Standard => HostProfile::Standard,
                    MemoryProfile::Dense => HostProfile::Dense,
                },
                keep_resident: config.keep_resident.map(ByteSize::bytes),
                release_idle_after: (config.release_idle_after_s > 0)
                    .then(|| Duration::from_secs(config.release_idle_after_s)),
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
                log_records_per_sec: config.limits.log_records_per_sec,
                incoming_calls_per_peer: config.cluster.incoming_calls_max,
                native: config.native.runtime(),
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
                egress: config.egress.runtime(),
                datasources: config
                    .datasources
                    .iter()
                    .map(|(name, d)| (name.clone(), runtime_datasource(&config, d)))
                    .collect(),
                buckets: config
                    .buckets
                    .iter()
                    .map(|(name, b)| {
                        // A cluster bucket is on disk on both nodes,
                        // replicated between them.
                        let durability = match b.scope {
                            BucketScope::Cluster => Durability::Replicated,
                            BucketScope::Node => b.durability,
                        };
                        let bucket = SharedBucket {
                            durability,
                            quota: b.quota.map(|q| q.bytes()),
                        };
                        (name.clone(), bucket)
                    })
                    .collect(),
            },
            extensions,
        )
        .map_err(internal)?;
        rt.check_formats()
            .await
            .map_err(|e| NodeError::Invalid(format!("{}: {e}", config.data_dir.display())))?;
        let apps = rt.start_apps(Vec::new()).await.map_err(internal)?;
        let stop = tokio::sync::watch::channel(false).0;
        let ingress = Ingress::open(&config, &apps, &stop).await?;
        let events = Events::new();
        let audit = Audit::new(store.audit_path(), events.clone());
        let gate = config.failover.is_some().then(|| {
            Arc::new(Gate::new(
                rt.clone(),
                apps.clone(),
                config.cluster_buckets(),
            ))
        });
        let node = Self {
            rt,
            apps,
            store,
            audit,
            events,
            logs,
            policy,
            change: tokio::sync::Mutex::new(()),
            state: std::sync::Mutex::new(NodeState {
                desired,
                ..NodeState::default()
            }),
            connections: std::sync::Mutex::default(),
            handlers: std::sync::Mutex::default(),
            extensions: std::sync::Mutex::default(),
            ingress,
            stop,
            quic: Arc::default(),
            #[cfg(feature = "tls")]
            operators: Arc::default(),
            gate,
            failover: std::sync::OnceLock::new(),
            leadership: tokio::sync::watch::channel(0).0,
            config,
        };
        tokio::spawn(events::forward(
            node.events.clone(),
            node.rt.instance_events(),
            node.rt.node_events(),
            node.stop.subscribe(),
        ));
        if let Some(binding) = &node.config.metrics {
            let listener = tokio::net::TcpListener::bind(binding.address)
                .await
                .map_err(|e| NodeError::Invalid(format!("metrics: {}: {e}", binding.address)))?;
            let (rt, ingress, quic) = (node.rt.clone(), node.ingress.clone(), node.quic.clone());
            let render = move || metrics_text(&rt, &ingress, &quic);
            tokio::spawn(super::server::serve_metrics(
                listener,
                render,
                node.stop.subscribe(),
            ));
        }
        node.restore().await;
        let desired = node.st().desired.clone();
        node.store.prune_compiled(&desired);
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
        self.ingress.stop();
        let names: Vec<String> = self.connection_apps().keys().cloned().collect();
        for name in names {
            self.stop_connections(&name);
        }
        let names: Vec<String> = self.handler_apps().keys().cloned().collect();
        for name in names {
            self.stop_handler(&name);
        }
        self.apps.stop().await;
    }

    /// Every app's metrics and the node's, in the Prometheus text format:
    /// the runtime's, then the HTTP ingress's.
    #[must_use]
    pub fn metrics(&self) -> String {
        metrics_text(&self.rt, &self.ingress, &self.quic)
    }

    /// Reload every HTTPS ingress's certificates from their files. An
    /// ingress whose files fail keeps its old certificates.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] naming each ingress that failed.
    pub fn reload_tls(&self) -> Result<Done, NodeError> {
        let (ingresses, mut failed) = self.ingress.reload_tls();
        #[cfg(feature = "tls")]
        if let Err(e) = self.reload_operators() {
            failed.push(e.to_string());
        }
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
            json!({ "ingresses": ingresses, "failed": failed }),
        );
        if !failed.is_empty() {
            return Err(NodeError::Invalid(failed.join("; ")));
        }
        Ok(Done {
            message: format!("reloaded {ingresses} HTTPS ingress(es)"),
        })
    }

    /// The admin socket this node's configuration names.
    #[must_use]
    pub fn socket(&self) -> std::path::PathBuf {
        self.config.socket()
    }
}

/// The runtime's metrics, then the HTTP ingress's, then the cluster
/// transport's.
fn metrics_text(
    rt: &Runtime,
    ingress: &Ingress,
    quic: &std::sync::Mutex<Option<QuicTransport>>,
) -> String {
    let mut text = rt.render_metrics();
    text.push_str(&ingress.metrics());
    let quic = quic
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(quic) = quic {
        text.push_str(&cluster_metrics(&quic.send_stats()));
    }
    if let Some(s) = rt.replication_status() {
        text.push_str(&replication_metrics(
            s.commit,
            s.last,
            s.partner_matched,
            s.alone,
        ));
    }
    text
}

/// The replicated log (`cluster` buckets) in the Prometheus text format:
/// its commit and last index, and on the primary how far the standby
/// trails and whether it commits alone.
fn replication_metrics(commit: u64, last: u64, matched: Option<u64>, alone: bool) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let mut gauge = |name: &str, help: &str, value: u64| {
        let _ = writeln!(
            text,
            "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
        );
    };
    gauge(
        "paddock_replication_commit_index",
        "The last replicated log entry applied to this node's cluster buckets.",
        commit,
    );
    gauge(
        "paddock_replication_last_index",
        "The last entry in this node's replicated log.",
        last,
    );
    if let Some(matched) = matched {
        gauge(
            "paddock_replication_partner_lag_entries",
            "On the primary, log entries the standby has not acknowledged.",
            last.saturating_sub(matched),
        );
        gauge(
            "paddock_replication_alone",
            "On the primary, 1 while it commits without the standby.",
            u64::from(alone),
        );
    }
    text
}

/// Each peer's data lane in the Prometheus text format.
fn cluster_metrics(stats: &[(u32, paddock_host::transport::LaneStats)]) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = writeln!(
        text,
        "# HELP paddock_cluster_send_queue_bytes Bytes of messages queued for another node.\n\
         # TYPE paddock_cluster_send_queue_bytes gauge"
    );
    for (node, s) in stats {
        let _ = writeln!(
            text,
            "paddock_cluster_send_queue_bytes{{node=\"{node}\"}} {}",
            s.queued_bytes
        );
    }
    let _ = writeln!(
        text,
        "# HELP paddock_cluster_send_refused_total Messages and notices refused for a congested link to another node.\n\
         # TYPE paddock_cluster_send_refused_total counter"
    );
    for (node, s) in stats {
        let _ = writeln!(
            text,
            "paddock_cluster_send_refused_total{{node=\"{node}\"}} {}",
            s.refused
        );
    }
    text
}

/// Without the `tls` feature there is no remote admin API.
#[cfg(not(feature = "tls"))]
impl Node {
    /// Refuse `[admin]`: this build has no TLS.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] if the config has an `[admin]` section.
    #[allow(clippy::unused_async)] // The same signature as with `tls`.
    pub async fn start_remote_admin(self: &Arc<Self>) -> Result<(), NodeError> {
        if self.config.admin.is_some() {
            return Err(NodeError::Invalid(
                "[admin] needs a paddock built with the `tls` feature".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod daemon_tests;

/// The host's form of a `[datasources.<name>]` table: its password secret
/// resolved to the secret's source.
fn runtime_datasource(
    config: &NodeConfig,
    d: &crate::node::config::DatasourceBinding,
) -> paddock_host::SqlDatasource {
    let mut ds = paddock_host::SqlDatasource::new(d.host.clone(), d.dbname.clone(), d.user.clone());
    ds.port = d.port;
    ds.password = d
        .password_secret
        .as_ref()
        .and_then(|name| config.secrets.get(name))
        .and_then(|s| match (&s.file, &s.env) {
            (Some(f), _) => Some(SecretSource::File(f.clone())),
            (None, Some(v)) => Some(SecretSource::Env(v.clone())),
            (None, None) => None,
        });
    ds.tls = match (d.tls, &d.ca_file) {
        (crate::node::config::DatasourceTls::Require, Some(ca)) => paddock_host::SqlTls::Require {
            ca_file: ca.clone(),
        },
        _ => paddock_host::SqlTls::Disable,
    };
    ds.pool_size = d.pool_size;
    ds.connect_timeout = Duration::from_millis(d.connect_timeout_ms);
    ds.acquire_timeout = Duration::from_millis(d.acquire_timeout_ms);
    ds.statement_timeout = Duration::from_millis(d.statement_timeout_ms);
    ds.max_result_bytes = d.max_result_bytes.bytes();
    ds
}
