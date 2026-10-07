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
}

fn default_max_instances() -> u32 {
    1_000
}

fn default_tick() -> u64 {
    10
}

impl NodeConfig {
    /// Parse a configuration from TOML.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] with the location of the problem.
    pub fn from_toml_str(text: &str) -> Result<Self, NodeError> {
        toml::from_str(text).map_err(|e| NodeError::Invalid(format!("node config: {e}")))
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
