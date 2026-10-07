//! The smaller manifest sections: `api`, `dependencies`,
//! `registered_names`, `messaging_acl`, `config`, `placement`, `health` and
//! `observability`.

use std::collections::BTreeMap;

use semver::VersionReq;
use serde::{Deserialize, Serialize};

use crate::manifest::validate::Validator;

/// Log severity, matching `wasi:logging` levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Trace.
    Trace,
    /// Debug.
    Debug,
    /// Info.
    #[default]
    Info,
    /// Warn.
    Warn,
    /// Error.
    Error,
    /// Critical.
    Critical,
}

/// The `api` section: the app's message catalog. Documentation in Phase 1;
/// messages are not checked against it yet.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Api {
    /// Message types the app accepts.
    pub messages: Vec<MessageSpec>,
}

impl Api {
    pub(crate) fn validate(&self, v: &mut Validator) {
        let mut seen = std::collections::BTreeSet::new();
        for (i, message) in self.messages.iter().enumerate() {
            v.at(format!("messages[{i}]"), |v| {
                if message.name.is_empty() {
                    v.error_at("name", "must not be empty");
                } else if !seen.insert(message.name.as_str()) {
                    v.error_at(
                        "name",
                        format!("'{}' is listed more than once", message.name),
                    );
                }
                if message.pattern == MessagePattern::Cast && message.response_schema.is_some() {
                    v.error_at("response_schema", "a cast has no response");
                }
            });
        }
    }
}

/// One message type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSpec {
    /// Schema id carried in the envelope.
    pub name: String,
    /// Call (expects a reply) or cast.
    pub pattern: MessagePattern,
    /// Envelope content type. Unset means `application/cbor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Path of the request JSON Schema in the bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_schema: Option<String>,
    /// Path of the response JSON Schema in the bundle. Calls only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<String>,
    /// What the message does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Whether a message expects a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePattern {
    /// Expects a reply.
    Call,
    /// Fire and forget.
    Cast,
}

/// An app this app needs running first. The list order is the start order
/// among dependencies; stop order is the reverse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    /// App name.
    pub app: String,
    /// Acceptable versions (`^2`, `>=1.2, <1.4`).
    pub version: VersionReq,
}

/// A name the app may register. Written as a plain string for a local name,
/// or `{ name, scope }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RegisteredNameRepr", into = "RegisteredNameRepr")]
pub struct RegisteredName {
    /// The name.
    pub name: String,
    /// Where it is registered.
    pub scope: NameScope,
}

/// Where a name is registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameScope {
    /// On the local node.
    #[default]
    Local,
    /// Unique across the cluster. Not supported until Phase 2.
    Global,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RegisteredNameRepr {
    Short(String),
    Full {
        name: String,
        #[serde(default)]
        scope: NameScope,
    },
}

impl From<RegisteredNameRepr> for RegisteredName {
    fn from(r: RegisteredNameRepr) -> Self {
        match r {
            RegisteredNameRepr::Short(name) => Self {
                name,
                scope: NameScope::Local,
            },
            RegisteredNameRepr::Full { name, scope } => Self { name, scope },
        }
    }
}

impl From<RegisteredName> for RegisteredNameRepr {
    fn from(r: RegisteredName) -> Self {
        match r.scope {
            NameScope::Local => Self::Short(r.name),
            NameScope::Global => Self::Full {
                name: r.name,
                scope: r.scope,
            },
        }
    }
}

/// The `messaging_acl` section. Not enforced until Phase 2.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessagingAcl {
    /// Apps or registered names that may call or cast this app. Empty means
    /// any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub accept_from: Vec<String>,
    /// Apps or registered names this app may call or cast. Empty means any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub may_call: Vec<String>,
}

/// The `config` section: the app's own configuration keys.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigSpec {
    /// Path of a JSON Schema for the keys, in the bundle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Default values, overridable by node configuration.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub defaults: BTreeMap<String, String>,
    /// Names of secrets the app reads. Never secret values. Not enforced
    /// until Phase 2.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

impl ConfigSpec {
    pub(crate) fn validate(&self, v: &mut Validator) {
        if self.defaults.keys().any(String::is_empty) {
            v.error_at("defaults", "keys must not be empty");
        }
        v.unique_names("secrets", self.secrets.iter().map(String::as_str));
    }
}

/// The `placement` section. Not enforced until Phase 3.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Placement {
    /// Node labels an instance's node must have.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub required_labels: BTreeMap<String, String>,
    /// Node labels preferred when choosing a node.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub preferred_labels: BTreeMap<String, String>,
    /// Run at most one instance across the cluster.
    pub singleton: bool,
    /// Spread instances across distinct values of this node label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread: Option<String>,
}

/// The `health` section: an optional `health` export polled by the host. Not
/// enforced until Phase 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Health {
    /// Time between checks.
    pub interval_ms: u64,
    /// Consecutive failures before the instance is killed as `unhealthy`.
    pub failure_threshold: u32,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            interval_ms: 10_000,
            failure_threshold: 3,
        }
    }
}

impl Health {
    pub(crate) fn validate(&self, v: &mut Validator) {
        if self.interval_ms == 0 {
            v.error_at("interval_ms", "must be greater than zero");
        }
        if self.failure_threshold == 0 {
            v.error_at("failure_threshold", "must be greater than zero");
        }
    }
}

/// The `observability` section.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Observability {
    /// Default log level for the app's records.
    pub log_level: LogLevel,
    /// Export the app's metrics. Not enforced until Phase 2.
    pub metrics: bool,
    /// Fraction of traces sampled, 0.0 to 1.0. Not enforced until Phase 2.
    pub trace_sampling: f64,
}

impl Observability {
    pub(crate) fn validate(&self, v: &mut Validator) {
        if !(0.0..=1.0).contains(&self.trace_sampling) {
            v.error_at("trace_sampling", "must be between 0.0 and 1.0");
        }
    }
}
