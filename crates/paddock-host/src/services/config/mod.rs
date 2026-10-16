//! `wasi:config`: an app's configuration values, read-only.
//!
//! The values are the manifest's `config.defaults` with the node's per-app
//! overrides merged in ([`ConfigSpec::values`]). Without the capability
//! granted, both functions return `io("access-denied")`: the interface has
//! no denial case of its own. Paddock's own host rather than
//! `wasmtime-wasi-config`, which cannot deny (spike S7).

use std::collections::BTreeMap;
use std::sync::Arc;

use paddock_bundle::manifest::{AppSpec, Capability};
use wasmtime::Result;
use wasmtime::component::Linker;

use super::{Ctx, HostState, InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;

mod bindings {
    // Generated code carries no doc comments.
    #![allow(missing_docs)]

    wasmtime::component::bindgen!({
        path: [
            "../../wit",
            "../../wit/vendor/wasi-logging",
            "../../wit/vendor/wasi-config",
            "../../wit/vendor/wasi-keyvalue",
            "../../wit/groups",
            "../../wit/watch",
            "../../wit/secrets",
            "../../wit/metrics",
            "../../wit/sql",
            "wit",
        ],
        world: "paddock:host/config",
        imports: { default: async | trappable },
    });
}

use bindings::wasi::config::store;

const DENIED: &str = "access-denied";

/// The config service.
pub(crate) struct Config;

/// What the manifest asks of config for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ConfigSpec {
    /// What `wasi:config` returns: the manifest's `config.defaults`, with
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
    const INTERFACES: &'static [&'static str] = &["wasi:config/store"];
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

    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> Result<()> {
        store::add_to_linker::<T, Ctx<Self>>(linker, T::service)
    }
}

impl store::Host for ServiceCtx<'_, Instance> {
    async fn get(
        &mut self,
        key: String,
    ) -> Result<std::result::Result<Option<String>, store::Error>> {
        if !self.state.granted {
            return Ok(Err(store::Error::Io(DENIED.into())));
        }
        Ok(Ok(self.state.values.get(&key).cloned()))
    }

    async fn get_all(
        &mut self,
    ) -> Result<std::result::Result<Vec<(String, String)>, store::Error>> {
        if !self.state.granted {
            return Ok(Err(store::Error::Io(DENIED.into())));
        }
        Ok(Ok(self
            .state
            .values
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()))
    }
}
