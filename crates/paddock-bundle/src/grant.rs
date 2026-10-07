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

/// How one import is linked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "link", content = "capability", rename_all = "snake_case")]
pub enum ImportLink {
    /// Granted to every app.
    Implicit,
    /// Linked to the host implementation of this capability.
    Granted(Capability),
    /// Linked to deny stubs; the capability was declared but not granted.
    Denied(Capability),
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
    operator_grants: &BTreeSet<Capability>,
) -> Result<EffectiveGrant, BundleError> {
    let declared: BTreeSet<Capability> = spec.capabilities.declared().into_iter().collect();
    let mut links = BTreeMap::new();
    let mut undeclared = Vec::new();
    let mut unknown = Vec::new();
    let mut imported = BTreeSet::new();

    for import in imports {
        match classify_import(import) {
            ImportClass::Implicit => {
                links.insert(import.clone(), ImportLink::Implicit);
            }
            ImportClass::Unknown => unknown.push(import.clone()),
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

    let (granted, denied) = imported
        .iter()
        .copied()
        .partition(|c| operator_grants.contains(c));
    Ok(EffectiveGrant {
        imports: links,
        granted,
        denied,
        unused: declared.difference(&imported).copied().collect(),
    })
}

#[cfg(test)]
#[path = "grant_tests.rs"]
mod grant_tests;
