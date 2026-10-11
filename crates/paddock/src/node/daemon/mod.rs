//! The node: a runtime, its apps, the deploy store and the operations the
//! admin API exposes.

mod cluster;
mod connections;
mod deploy;
mod queries;
mod upgrade;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::api::Done;
use paddock_bundle::OperatorPolicy;
use paddock_host::failover::FailoverHandle;
use paddock_host::http::{HttpIngress, IngressListen, IngressTls, TlsCertificate};
use paddock_host::services::{Extension, StartedExtensions};
use paddock_host::transport::quic::QuicTransport;
use paddock_host::{
    Apps, HostObjects, MemoryProfile as HostProfile, Role, Runtime, RuntimeConfig, SecretSource,
    SharedBucket,
};
use serde_json::json;

use super::audit::Audit;
use super::config::{ClusterRole, MemoryProfile, NodeConfig};
use super::error::{NodeError, internal};
use super::gate::Gate;
use super::logring::LogRing;
use super::store::{State, Store};

use connections::ConnectionApp;

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
                log_records_per_sec: config.limits.log_records_per_sec,
                incoming_calls_per_peer: config.cluster.incoming_calls_max,
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
            change: tokio::sync::Mutex::new(()),
            state: std::sync::Mutex::new(NodeState {
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

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod daemon_tests;
