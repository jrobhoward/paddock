//! The effective grant: what a manifest declares, intersected with what the
//! operator grants.
//!
//! A declared capability the operator grants works. One the operator does
//! not grant answers `access-denied`, so the app can degrade. What an app
//! does not declare is refused when it asks.

use std::collections::BTreeSet;

use crate::manifest::{AppSpec, Capability};
use serde::Serialize;

use crate::policy::Granted;

/// The permissions an app runs with, recorded with the deployment for audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveGrant {
    /// Capabilities declared and granted.
    pub granted: BTreeSet<Capability>,
    /// Capabilities declared but not granted.
    pub denied: BTreeSet<Capability>,
    /// Extension packages declared and granted.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub granted_extensions: BTreeSet<String>,
    /// Extension packages declared but not granted.
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
    /// Secrets declared and granted, with `bulkhead:secrets` granted.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub granted_secrets: BTreeSet<String>,
    /// Secrets declared but not granted (reading one returns
    /// `access-denied`).
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub denied_secrets: BTreeSet<String>,
    /// SQL datasources declared and granted, with `bulkhead:sql` granted.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub granted_datasources: BTreeSet<String>,
    /// SQL datasources declared but not granted (using one returns
    /// `access-denied`).
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub denied_datasources: BTreeSet<String>,
}

/// The effective grant of `spec` given the capabilities the operator grants
/// this app.
#[must_use]
pub fn effective_grant(spec: &AppSpec, operator: &Granted) -> EffectiveGrant {
    let operator_grants = &operator.capabilities;
    let (granted_extensions, denied_extensions) = spec
        .capabilities
        .extensions
        .keys()
        .cloned()
        .partition(|package| operator.extensions.contains(package));
    let (granted, denied): (BTreeSet<Capability>, BTreeSet<Capability>) = spec
        .capabilities
        .declared()
        .into_iter()
        .partition(|c| operator_grants.contains(c));
    let kv = granted.contains(&Capability::KeyValue);
    let (granted_buckets, denied_buckets) = spec
        .capabilities
        .keyvalue
        .iter()
        .flat_map(|scope| scope.shared.keys().cloned())
        .partition(|name| kv && operator.buckets.contains(name));
    let secrets_on = granted.contains(&Capability::Secrets);
    let (granted_secrets, denied_secrets) = spec
        .capabilities
        .secrets
        .iter()
        .flat_map(|scope| scope.secrets.iter().cloned())
        .partition(|name| secrets_on && operator.secrets.contains(name));
    let sql_on = granted.contains(&Capability::Sql);
    let (granted_datasources, denied_datasources) = spec
        .capabilities
        .sql
        .iter()
        .flat_map(|scope| scope.datasources.keys().cloned())
        .partition(|name| sql_on && operator.datasources.contains(name));
    EffectiveGrant {
        granted,
        denied,
        granted_extensions,
        denied_extensions,
        granted_buckets,
        denied_buckets,
        granted_secrets,
        denied_secrets,
        granted_datasources,
        denied_datasources,
    }
}

#[cfg(test)]
#[path = "grant_tests.rs"]
mod grant_tests;
