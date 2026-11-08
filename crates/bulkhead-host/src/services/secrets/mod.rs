//! `bulkhead:secrets`: values an operator binds on the node, read by name.
//!
//! An instance reads only names its manifest lists and the operator's
//! policy grants ([`SecretsSpec::names`], narrowed by
//! [`InstanceSpec::with_granted_secrets`](crate::InstanceSpec::with_granted_secrets)),
//! with the capability granted. The node binds each name to a source
//! ([`SecretSource`]), read on every call so a rotated file is seen at
//! once. Values are never logged; errors name the secret, never its value.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bulkhead_bundle::manifest::{AppSpec, Capability};
use bulkhead_native::protocol::SecretError;

use super::{InstanceInfo, Service};
use crate::config::{RuntimeConfig, SecretSource};
use crate::error::HostError;

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
}

/// Read `source`.
async fn read(name: &str, source: &SecretSource) -> std::result::Result<Vec<u8>, SecretError> {
    match source {
        SecretSource::File(path) => {
            let path = path.clone();
            tokio::task::spawn_blocking(move || std::fs::read(path))
                .await
                .map_err(|_| SecretError::Unavailable(format!("secret '{name}': read failed")))?
                .map_err(|e| {
                    SecretError::Unavailable(format!("secret '{name}': file: {}", e.kind()))
                })
        }
        SecretSource::Env(var) => std::env::var_os(var)
            .map(|v| v.into_encoded_bytes())
            .ok_or_else(|| {
                SecretError::Unavailable(format!("secret '{name}': variable {var} is not set"))
            }),
    }
}

impl Instance {
    /// The secret `name`, if the manifest lists it and the operator granted
    /// it: what a component's `bulkhead:secrets` import and a native app's
    /// request both read (ROADMAP Z9b).
    pub(crate) async fn get_secret(&self, name: &str) -> std::result::Result<Vec<u8>, SecretError> {
        if !self.granted || !self.names.iter().any(|n| n == name) {
            return Err(SecretError::AccessDenied);
        }
        let Some(source) = self.sources.get(name).cloned() else {
            return Err(SecretError::NotFound);
        };
        read(name, &source).await
    }
}
