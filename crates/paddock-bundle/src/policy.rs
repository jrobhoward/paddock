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
//! buckets = ["points"]          # shared buckets, from the node's [buckets]
//!
//! # Granted to every app signed by this key.
//! [signers.payments-team]
//! key = "RWQ..."
//! grant = ["wasi:http/outgoing-handler"]
//! ```
//!
//! A `grant` list holds capability keys and extension package names
//! (`"example:greeting"`) alike.
//!
//! Policy grants or withholds whole capabilities, and each shared keyvalue
//! bucket by name (`buckets`; it needs `wasi:keyvalue` granted too). Other
//! scopes an app requests (its own buckets, preopens, hosts) are used as
//! written; narrowing them by policy is not supported.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::manifest::{Capability, is_extension_key};
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
    /// Capabilities and extensions granted, by key (`wasi:keyvalue`,
    /// `example:greeting`).
    pub grant: Granted,
    /// Shared keyvalue buckets granted, by name.
    pub buckets: BTreeSet<String>,
}

/// Capabilities granted to apps signed by one key.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignerGrants {
    /// The signer's minisign public key, base64.
    pub key: String,
    /// Capabilities and extensions granted, by key.
    #[serde(default)]
    pub grant: Granted,
    /// Shared keyvalue buckets granted, by name.
    #[serde(default)]
    pub buckets: BTreeSet<String>,
}

/// Granted capabilities and extension packages. Written in policy as one
/// list of keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Vec<String>")]
pub struct Granted {
    /// Built-in capabilities.
    pub capabilities: BTreeSet<Capability>,
    /// Extension packages (`example:greeting`).
    pub extensions: BTreeSet<String>,
    /// Shared keyvalue buckets, from the policy's `buckets` lists.
    pub buckets: BTreeSet<String>,
}

impl Granted {
    fn extend(&mut self, other: &Self, buckets: &BTreeSet<String>) {
        self.capabilities.extend(&other.capabilities);
        self.extensions.extend(other.extensions.iter().cloned());
        self.buckets.extend(buckets.iter().cloned());
    }
}

impl TryFrom<Vec<String>> for Granted {
    type Error = String;

    fn try_from(keys: Vec<String>) -> Result<Self, String> {
        let mut granted = Self::default();
        for key in keys {
            if let Ok(capability) = Capability::try_from(key.clone()) {
                granted.capabilities.insert(capability);
            } else if is_extension_key(&key) {
                granted.extensions.insert(key);
            } else {
                return Err(format!(
                    "unknown capability '{key}' (not a capability key or an extension package outside wasi and paddock)"
                ));
            }
        }
        Ok(granted)
    }
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
    pub fn granted(&self, app: &str, signer_key: &str) -> Granted {
        let mut granted = Granted::default();
        granted.extend(&self.defaults.grant, &self.defaults.buckets);
        if let Some(entry) = self.apps.get(app) {
            granted.extend(&entry.grant, &entry.buckets);
        }
        for signer in self.signers.values().filter(|s| s.key == signer_key) {
            granted.extend(&signer.grant, &signer.buckets);
        }
        granted
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod policy_tests;
