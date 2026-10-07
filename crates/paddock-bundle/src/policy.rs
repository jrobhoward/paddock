//! Operator policy: which capabilities the operator grants, and to whom.
//!
//! Phase 1 format (TOML). Grants add up: an app gets the defaults, plus its
//! own entry, plus the entry of every signer whose key verified its bundle.
//!
//! ```toml
//! # Granted to every app.
//! [defaults]
//! grant = ["wasi:logging", "wasi:config"]
//!
//! # Granted to one app, by name.
//! [apps.orders]
//! grant = ["wasi:keyvalue", "wasi:filesystem"]
//!
//! # Granted to every app signed by this key.
//! [signers.payments-team]
//! key = "RWQ..."
//! grant = ["wasi:http/outgoing-handler"]
//! ```
//!
//! Policy grants or withholds whole capabilities. The scope an app requests
//! (buckets, preopens, hosts) is used as written; narrowing scopes by policy
//! is not supported in Phase 1.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::manifest::Capability;
use serde::Deserialize;

use crate::error::BundleError;

/// The operator's grants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OperatorPolicy {
    /// Granted to every app.
    pub defaults: Grants,
    /// Granted to an app, by app name.
    pub apps: BTreeMap<String, Grants>,
    /// Granted to apps signed by a key, by a label for the signer.
    pub signers: BTreeMap<String, SignerGrants>,
}

/// A set of granted capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Grants {
    /// Capabilities granted, by key (`wasi:keyvalue`).
    pub grant: BTreeSet<Capability>,
}

/// Capabilities granted to apps signed by one key.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignerGrants {
    /// The signer's minisign public key, base64.
    pub key: String,
    /// Capabilities granted, by key.
    #[serde(default)]
    pub grant: BTreeSet<Capability>,
}

impl OperatorPolicy {
    /// Parse a policy from TOML text.
    ///
    /// # Errors
    ///
    /// [`BundleError::Policy`] with the location of the problem.
    pub fn from_toml_str(text: &str) -> Result<Self, BundleError> {
        toml::from_str(text).map_err(|e| BundleError::Policy(e.to_string()))
    }

    /// Read and parse a policy file.
    ///
    /// # Errors
    ///
    /// [`BundleError::Policy`] if the file cannot be read or parsed.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, BundleError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| BundleError::Policy(format!("{}: {e}", path.display())))?;
        Self::from_toml_str(&text)
    }

    /// Everything granted to `app` whose bundle was verified by
    /// `signer_key`.
    #[must_use]
    pub fn granted(&self, app: &str, signer_key: &str) -> BTreeSet<Capability> {
        let mut granted = self.defaults.grant.clone();
        if let Some(entry) = self.apps.get(app) {
            granted.extend(&entry.grant);
        }
        for signer in self.signers.values().filter(|s| s.key == signer_key) {
            granted.extend(&signer.grant);
        }
        granted
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod policy_tests;
