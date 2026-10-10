//! `paddock:secrets`: values an operator binds on the node, read by name.
//!
//! An instance reads only names its manifest lists and the operator's
//! policy grants ([`SecretsSpec::names`], narrowed by
//! [`InstanceSpec::with_granted_secrets`](crate::InstanceSpec::with_granted_secrets)),
//! with the capability granted. The node binds each name to a source
//! ([`SecretSource`]), read on every call so a rotated file is seen at
//! once. Values are never logged; errors name the secret, never its value.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use paddock_bundle::manifest::{AppSpec, Capability};
use wasmtime::Result;
use wasmtime::component::Linker;

use super::{Ctx, HostState, InstanceInfo, Service, ServiceCtx};
use crate::config::{RuntimeConfig, SecretSource};
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
            "wit",
        ],
        world: "paddock:host/secrets",
        imports: { default: async | trappable },
    });
}

use bindings::paddock::secrets::store;

/// The secrets service.
pub(crate) struct Secrets;

/// What the manifest asks of secrets for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SecretsSpec {
    /// Names the instance may read: the manifest's list, narrowed to the
    /// operator's grant.
    pub names: BTreeSet<String>,
}

/// Secrets state for one instance.
pub(crate) struct Instance {
    sources: Arc<BTreeMap<String, SecretSource>>,
    names: BTreeSet<String>,
    granted: bool,
}

impl Service for Secrets {
    const CAPABILITY: Capability = Capability::Secrets;
    const INTERFACES: &'static [&'static str] = &["paddock:secrets/store"];
    type Spec = SecretsSpec;
    type Shared = BTreeMap<String, SecretSource>;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> SecretsSpec {
        SecretsSpec {
            names: manifest
                .capabilities
                .secrets
                .iter()
                .flat_map(|s| s.secrets.iter().cloned())
                .collect(),
        }
    }

    fn shared(config: &RuntimeConfig) -> Result<BTreeMap<String, SecretSource>, HostError> {
        Ok(config.secrets.clone())
    }

    fn instance(
        shared: &Arc<BTreeMap<String, SecretSource>>,
        spec: &SecretsSpec,
        info: &InstanceInfo<'_>,
    ) -> Instance {
        Instance {
            sources: shared.clone(),
            names: spec.names.clone(),
            granted: info.granted,
        }
    }

    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> Result<()> {
        store::add_to_linker::<T, Ctx<Self>>(linker, T::service)
    }
}

/// Read `source`.
async fn read(name: &str, source: &SecretSource) -> std::result::Result<Vec<u8>, store::Error> {
    match source {
        SecretSource::File(path) => {
            let path = path.clone();
            tokio::task::spawn_blocking(move || std::fs::read(path))
                .await
                .map_err(|_| store::Error::Unavailable(format!("secret '{name}': read failed")))?
                .map_err(|e| {
                    store::Error::Unavailable(format!("secret '{name}': file: {}", e.kind()))
                })
        }
        SecretSource::Env(var) => std::env::var_os(var)
            .map(|v| v.into_encoded_bytes())
            .ok_or_else(|| {
                store::Error::Unavailable(format!("secret '{name}': variable {var} is not set"))
            }),
    }
}

impl store::Host for ServiceCtx<'_, Instance> {
    async fn get(&mut self, name: String) -> Result<std::result::Result<Vec<u8>, store::Error>> {
        let state = &*self.state;
        if !state.granted || !state.names.contains(&name) {
            return Ok(Err(store::Error::AccessDenied));
        }
        let Some(source) = state.sources.get(&name).cloned() else {
            return Ok(Err(store::Error::NotFound));
        };
        let started = Instant::now();
        let value = read(&name, &source).await;
        self.add_host_time(started.elapsed());
        Ok(value)
    }
}
