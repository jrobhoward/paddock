//! Node configuration (TOML).
//!
//! ```toml
//! node = 1
//! data_dir = "/var/lib/paddock"
//! # admin_socket = "/var/lib/paddock/admin.sock"   (the default)
//! trusted_keys = ["RWQ..."]
//! policy = "/etc/paddock/policy.toml"
//! max_instances = 1000
//! memory_profile = "standard"   # or "dense"
//!
//! [volumes]
//! orders-data = "/srv/paddock/volumes/orders-data"
//!
//! # Node-level config overrides, per app, on top of the manifest defaults.
//! [config.orders]
//! "db.host" = "pg.internal"
//!
//! [cluster]
//! role = "member"               # or "witness": runs no apps
//! heartbeat_interval_ms = 1000
//! node_timeout_ms = 5000
//!
//! # Host objects per instance: the value for apps that set none, and the
//! # most a manifest may ask for.
//! [limits]
//! timers_max = 10000
//! monitors_max = 10000
//! links_max = 10000
//! pending_calls_max = 10000
//! groups_max = 1000
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::error::NodeError;

/// Address-space profile, as in the host's `MemoryProfile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryProfile {
    /// Large reservations; up to a few thousand instances.
    #[default]
    Standard,
    /// 128 MiB reservations; dense nodes.
    Dense,
}

/// The node's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Node id, carried in every pid.
    #[serde(default)]
    pub node: u32,
    /// Where the deploy store, keyvalue data and audit log live.
    pub data_dir: PathBuf,
    /// The admin API socket. Defaults to `<data_dir>/admin.sock`.
    #[serde(default)]
    pub admin_socket: Option<PathBuf>,
    /// Minisign public keys whose signatures the node accepts.
    #[serde(default)]
    pub trusted_keys: Vec<String>,
    /// Operator policy file (TOML). Without one, nothing beyond the
    /// implicit interfaces is granted.
    #[serde(default)]
    pub policy: Option<PathBuf>,
    /// Most instances alive at once.
    #[serde(default = "default_max_instances")]
    pub max_instances: u32,
    /// Address-space profile.
    #[serde(default)]
    pub memory_profile: MemoryProfile,
    /// Epoch tick, in milliseconds.
    #[serde(default = "default_tick")]
    pub epoch_tick_ms: u64,
    /// Host directories backing volume names in manifests.
    #[serde(default)]
    pub volumes: BTreeMap<String, PathBuf>,
    /// Config overrides per app.
    #[serde(default)]
    pub config: BTreeMap<String, BTreeMap<String, String>>,
    /// Addresses for the listeners apps declare, by listener name.
    #[serde(default)]
    pub listeners: BTreeMap<String, ListenerBinding>,
    /// This node's part in a cluster.
    #[serde(default)]
    pub cluster: ClusterConfig,
    /// Host-object limits per instance.
    #[serde(default)]
    pub limits: NodeLimits,
}

/// The `[limits]` section: host objects per instance, as the manifest's
/// `limits` fields of the same names. Each is the value for apps that set
/// none and the most a manifest may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeLimits {
    /// Pending `send-after` timers.
    #[serde(default = "default_host_objects")]
    pub timers_max: u32,
    /// Monitors held on other instances.
    #[serde(default = "default_host_objects")]
    pub monitors_max: u32,
    /// Links.
    #[serde(default = "default_host_objects")]
    pub links_max: u32,
    /// Calls answered `noreply` and not yet replied to.
    #[serde(default = "default_host_objects")]
    pub pending_calls_max: u32,
    /// Process groups joined.
    #[serde(default = "default_groups")]
    pub groups_max: u32,
}

impl Default for NodeLimits {
    fn default() -> Self {
        Self {
            timers_max: default_host_objects(),
            monitors_max: default_host_objects(),
            links_max: default_host_objects(),
            pending_calls_max: default_host_objects(),
            groups_max: default_groups(),
        }
    }
}

fn default_groups() -> u32 {
    1_000
}

fn default_host_objects() -> u32 {
    10_000
}

/// What a node does in the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterRole {
    /// Runs apps.
    #[default]
    Member,
    /// Runs no apps: a third node for failover decisions (ROADMAP D10).
    Witness,
}

/// The `[cluster]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    /// What this node does.
    #[serde(default)]
    pub role: ClusterRole,
    /// How often this node tells each peer it is alive, in milliseconds.
    #[serde(default = "default_heartbeat_ms")]
    pub heartbeat_interval_ms: u64,
    /// Silence after which a peer's session ends, in milliseconds. More
    /// than `heartbeat_interval_ms`.
    #[serde(default = "default_node_timeout_ms")]
    pub node_timeout_ms: u64,
    /// Where other nodes reach this one. Used from the QUIC transport on
    /// (ROADMAP D1).
    #[serde(default)]
    pub address: Option<String>,
    /// Nodes to join through. Used from the QUIC transport on (ROADMAP D1).
    #[serde(default)]
    pub seeds: Vec<String>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            role: ClusterRole::default(),
            heartbeat_interval_ms: default_heartbeat_ms(),
            node_timeout_ms: default_node_timeout_ms(),
            address: None,
            seeds: Vec::new(),
        }
    }
}

fn default_heartbeat_ms() -> u64 {
    1_000
}

fn default_node_timeout_ms() -> u64 {
    5_000
}

/// Where a listener an app declares listens on this node.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerBinding {
    /// The address to bind, `ip:port`.
    pub address: std::net::SocketAddr,
    /// Tasks accepting connections at once.
    #[serde(default = "default_acceptors")]
    pub acceptors: usize,
}

fn default_acceptors() -> usize {
    4
}

fn default_max_instances() -> u32 {
    1_000
}

fn default_tick() -> u64 {
    2
}

impl NodeConfig {
    /// Parse a configuration from TOML.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] with the location of the problem.
    pub fn from_toml_str(text: &str) -> Result<Self, NodeError> {
        let config: Self =
            toml::from_str(text).map_err(|e| NodeError::Invalid(format!("node config: {e}")))?;
        let cluster = &config.cluster;
        if cluster.heartbeat_interval_ms == 0
            || cluster.node_timeout_ms <= cluster.heartbeat_interval_ms
        {
            return Err(NodeError::Invalid(format!(
                "node config: cluster.node_timeout_ms ({}) must be more than \
                 cluster.heartbeat_interval_ms ({}), which must be more than 0",
                cluster.node_timeout_ms, cluster.heartbeat_interval_ms
            )));
        }
        let limits = &config.limits;
        for (name, value) in [
            ("timers_max", limits.timers_max),
            ("monitors_max", limits.monitors_max),
            ("links_max", limits.links_max),
            ("pending_calls_max", limits.pending_calls_max),
            ("groups_max", limits.groups_max),
        ] {
            if value == 0 {
                return Err(NodeError::Invalid(format!(
                    "node config: limits.{name} must be more than 0"
                )));
            }
        }
        Ok(config)
    }

    /// Read and parse a configuration file.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] if the file cannot be read or parsed.
    pub fn from_file(path: &Path) -> Result<Self, NodeError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| NodeError::Invalid(format!("{}: {e}", path.display())))?;
        Self::from_toml_str(&text)
    }

    /// The admin socket path.
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.admin_socket
            .clone()
            .unwrap_or_else(|| self.data_dir.join("admin.sock"))
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
