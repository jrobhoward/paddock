//! Deploy-time import checks and the effective grant.
//!
//! Effective permission = component imports ∩ manifest declaration ∩
//! operator grant:
//!
//! - An import that is neither implicit nor covered by a declared
//!   capability refuses the deploy, as does one no host provides.
//! - A declared, imported capability the operator grants is linked to the
//!   real host implementation.
//! - A declared, imported capability the operator does not grant is linked
//!   to deny stubs that return `access-denied`, so the app can degrade.
//! - A declared capability the component never imports is unused; a
//!   component imports only what its code calls.

use std::collections::{BTreeMap, BTreeSet};

use crate::manifest::{AppSpec, Capability, ImportClass, classify_import};
use serde::Serialize;

use crate::error::BundleError;
use crate::policy::Granted;

/// How one import is linked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "link", content = "capability", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ImportLink {
    /// Granted to every app.
    Implicit,
    /// Linked to the host implementation of this capability.
    Granted(Capability),
    /// Linked to deny stubs; the capability was declared but not granted.
    Denied(Capability),
    /// Linked to the node's service for this extension package, if it has
    /// one (deny stubs otherwise).
    GrantedExtension(String),
    /// Linked to deny stubs; the extension was declared but not granted.
    DeniedExtension(String),
}

/// The permissions an app runs with, recorded with the deployment for audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveGrant {
    /// How each import is linked, by full import name.
    pub imports: BTreeMap<String, ImportLink>,
    /// Capabilities imported, declared and granted.
    pub granted: BTreeSet<Capability>,
    /// Capabilities imported and declared but not granted.
    pub denied: BTreeSet<Capability>,
    /// Capabilities declared but not imported.
    pub unused: BTreeSet<Capability>,
    /// Extension packages imported, declared and granted.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub granted_extensions: BTreeSet<String>,
    /// Extension packages imported and declared but not granted.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub denied_extensions: BTreeSet<String>,
    /// Shared keyvalue buckets declared and granted, with `wasi:keyvalue`
    /// granted.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub granted_buckets: BTreeSet<String>,
    /// Shared keyvalue buckets declared but not granted (opening one
    /// returns `access-denied`).
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub denied_buckets: BTreeSet<String>,
}

/// Check `imports` against `spec` and compute the effective grant given the
/// capabilities the operator grants this app.
///
/// # Errors
///
/// [`BundleError::ImportsRefused`] listing every import that is undeclared
/// or that no host provides.
pub fn effective_grant(
    imports: &[String],
    spec: &AppSpec,
    operator: &Granted,
) -> Result<EffectiveGrant, BundleError> {
    let operator_grants = &operator.capabilities;
    let declared: BTreeSet<Capability> = spec.capabilities.declared().into_iter().collect();
    let mut links = BTreeMap::new();
    let mut undeclared = Vec::new();
    let mut unknown = Vec::new();
    let mut imported = BTreeSet::new();
    let (mut granted_extensions, mut denied_extensions) = (BTreeSet::new(), BTreeSet::new());

    for import in imports {
        match classify_import(import) {
            ImportClass::Implicit => {
                links.insert(import.clone(), ImportLink::Implicit);
            }
            ImportClass::Unknown => unknown.push(import.clone()),
            ImportClass::Extension(package) => {
                if !spec.capabilities.extensions.contains_key(&package) {
                    undeclared.push(format!("{import} (needs extensions.\"{package}\")"));
                } else if operator.extensions.contains(&package) {
                    links.insert(
                        import.clone(),
                        ImportLink::GrantedExtension(package.clone()),
                    );
                    granted_extensions.insert(package);
                } else {
                    links.insert(import.clone(), ImportLink::DeniedExtension(package.clone()));
                    denied_extensions.insert(package);
                }
            }
            ImportClass::Capability(candidates) => {
                let declared_here: Vec<Capability> = candidates
                    .iter()
                    .copied()
                    .filter(|c| declared.contains(c))
                    .collect();
                if declared_here.is_empty() {
                    let keys: Vec<&str> = candidates.iter().map(|c| c.key()).collect();
                    undeclared.push(format!("{import} (needs {})", keys.join(" or ")));
                    continue;
                }
                imported.extend(&declared_here);
                let link = declared_here
                    .iter()
                    .find(|c| operator_grants.contains(c))
                    .map_or(ImportLink::Denied(declared_here[0]), |c| {
                        ImportLink::Granted(*c)
                    });
                links.insert(import.clone(), link);
            }
        }
    }

    if !undeclared.is_empty() || !unknown.is_empty() {
        return Err(BundleError::ImportsRefused {
            undeclared,
            unknown,
        });
    }

    let (granted, denied): (BTreeSet<Capability>, BTreeSet<Capability>) = imported
        .iter()
        .copied()
        .partition(|c| operator_grants.contains(c));
    let kv = granted.contains(&Capability::KeyValue);
    let (granted_buckets, denied_buckets) = spec
        .capabilities
        .keyvalue
        .iter()
        .flat_map(|scope| scope.shared.keys().cloned())
        .partition(|name| kv && operator.buckets.contains(name));
    Ok(EffectiveGrant {
        imports: links,
        granted,
        denied,
        unused: declared.difference(&imported).copied().collect(),
        granted_extensions,
        denied_extensions,
        granted_buckets,
        denied_buckets,
    })
}

#[cfg(test)]
#[path = "grant_tests.rs"]
mod grant_tests;
