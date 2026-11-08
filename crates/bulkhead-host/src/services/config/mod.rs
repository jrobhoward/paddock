//! `bulkhead:config`: an app's configuration values, read-only.
//!
//! The values are the manifest's `config.defaults` with the node's per-app
//! overrides merged in ([`ConfigSpec::values`]). Without the capability
//! granted, both functions return `io("access-denied")`: the interface has
//! no denial case of its own. Bulkhead's own host rather than
//! `wasmtime-wasi-config`, which cannot deny (spike S7).

use std::collections::BTreeMap;
use std::sync::Arc;

use bulkhead_bundle::manifest::{AppSpec, Capability};

use super::{InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;

/// The config service.
pub(crate) struct Config;

/// What the manifest asks of config for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ConfigSpec {
    /// What `bulkhead:config` returns: the manifest's `config.defaults`, with
    /// node overrides merged in by the caller.
    pub values: BTreeMap<String, String>,
}

/// Config state for one instance.
pub(crate) struct Instance {
    values: BTreeMap<String, String>,
    granted: bool,
}

impl Service for Config {
    const CAPABILITY: Capability = Capability::Config;
    type Spec = ConfigSpec;
    type Shared = ();
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> ConfigSpec {
        ConfigSpec {
            values: manifest.config.defaults.clone(),
        }
    }

    fn shared(_config: &RuntimeConfig) -> Result<(), HostError> {
        Ok(())
    }

    fn instance(_shared: &Arc<()>, spec: &ConfigSpec, info: &InstanceInfo<'_>) -> Instance {
        Instance {
            values: spec.values.clone(),
            granted: info.granted,
        }
    }
}

impl ServiceCtx<'_, Instance> {
    /// A configuration value for a native app (`None` for every value),
    /// as a component's `bulkhead:config` import reads it.
    pub(crate) fn native(
        &self,
        key: Option<&str>,
    ) -> std::result::Result<Vec<(String, String)>, bulkhead_native::protocol::ConfigError> {
        if !self.state.granted {
            return Err(bulkhead_native::protocol::ConfigError::AccessDenied);
        }
        Ok(match key {
            Some(key) => self
                .state
                .values
                .get_key_value(key)
                .map(|(k, v)| (k.clone(), v.clone()))
                .into_iter()
                .collect(),
            None => self
                .state
                .values
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        })
    }
}
