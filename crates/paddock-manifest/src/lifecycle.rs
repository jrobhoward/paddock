//! How an app is started, supervised, scaled and upgraded: the `kind`,
//! `lifecycle`, `supervision`, `instances` and `upgrade` sections.

use std::fmt;
use std::str::FromStr;

use semver::VersionReq;
use serde::{Deserialize, Serialize};

use crate::validate::Validator;

/// What the bundle runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Kind {
    /// A WebAssembly component implementing `paddock:runtime/app`.
    #[default]
    WasmComponent,
    /// A rustbridge native library run as a separate OS process. Not
    /// supported in Phase 1.
    NativeProcess,
}

/// When a supervisor restarts a child that exited (OTP restart types).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartType {
    /// Always restart.
    #[default]
    Permanent,
    /// Restart only after an abnormal exit.
    Transient,
    /// Never restart.
    Temporary,
}

/// The `lifecycle` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lifecycle {
    /// Restart type of the app's instances under its root supervisor.
    pub restart: RestartType,
    /// How long `terminate` may run before the instance is killed.
    pub shutdown_timeout_ms: u64,
    /// How long `init` may run before it counts as a failed start.
    pub init_timeout_ms: u64,
    /// Passed to `init`. Encoded by the host before the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_args: Option<serde_json::Value>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            restart: RestartType::Permanent,
            shutdown_timeout_ms: 5_000,
            init_timeout_ms: 5_000,
            start_args: None,
        }
    }
}

impl Lifecycle {
    pub(crate) fn validate(&self, v: &mut Validator) {
        if self.shutdown_timeout_ms == 0 {
            v.error_at("shutdown_timeout_ms", "must be greater than zero");
        }
        if self.init_timeout_ms == 0 {
            v.error_at("init_timeout_ms", "must be greater than zero");
        }
    }
}

/// Supervisor restart strategy (OTP strategies).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Restart only the child that exited.
    #[default]
    OneForOne,
    /// Restart every child when one exits.
    OneForAll,
    /// Restart the child that exited and every child started after it.
    RestForOne,
    /// A dynamic pool of identical children from a single child spec.
    SimpleOneForOne,
}

/// Delay between restarts, growing on repeated failure. An addition beyond
/// OTP, to avoid hot crash loops against a failing dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Backoff {
    /// Delay before the first restart.
    pub initial_ms: u64,
    /// Upper bound on the delay; it doubles on each consecutive restart.
    pub max_ms: u64,
    /// Randomize each delay so restarts across instances don't line up.
    pub jitter: bool,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial_ms: 100,
            max_ms: 5_000,
            jitter: true,
        }
    }
}

/// A child started by a supervisor app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildSpec {
    /// Identifies the child within its supervisor.
    pub id: String,
    /// The app to start.
    pub app: String,
    /// Overrides the child app's own `lifecycle.restart`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<RestartType>,
    /// Overrides the child app's own `lifecycle.shutdown_timeout_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_timeout_ms: Option<u64>,
    /// Overrides the child app's own `lifecycle.start_args`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_args: Option<serde_json::Value>,
}

/// The `supervision` section: the app's root supervisor and, for supervisor
/// apps, the children it starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Supervision {
    /// Restart strategy.
    pub strategy: Strategy,
    /// Most restarts allowed within `period_ms` before the supervisor gives
    /// up and escalates.
    pub intensity: u32,
    /// Window for `intensity`.
    pub period_ms: u64,
    /// Delay between restarts.
    pub backoff: Backoff,
    /// Children, in start order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildSpec>,
}

impl Default for Supervision {
    fn default() -> Self {
        Self {
            strategy: Strategy::OneForOne,
            intensity: 5,
            period_ms: 10_000,
            backoff: Backoff::default(),
            children: Vec::new(),
        }
    }
}

impl Supervision {
    pub(crate) fn validate(&self, v: &mut Validator) {
        if self.period_ms == 0 {
            v.error_at("period_ms", "must be greater than zero");
        }
        if self.backoff.max_ms < self.backoff.initial_ms {
            v.error_at("backoff.max_ms", "must be at least backoff.initial_ms");
        }
        v.unique_names("children", self.children.iter().map(|c| c.id.as_str()));
        for (i, child) in self.children.iter().enumerate() {
            v.at(format!("children[{i}]"), |v| {
                v.name("app", &child.app);
                if child.shutdown_timeout_ms == Some(0) {
                    v.error_at("shutdown_timeout_ms", "must be greater than zero");
                }
            });
        }
        if self.strategy == Strategy::SimpleOneForOne && self.children.len() != 1 {
            v.error_at(
                "children",
                "simple_one_for_one needs exactly one child spec, the template for the pool",
            );
        }
    }
}

/// How messages sent to the app's registered name pick an instance in a pool.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Routing {
    /// Each message goes to the next instance in turn. Written
    /// `"round-robin"`.
    #[default]
    RoundRobin,
    /// Messages with the same key go to the same instance. Written
    /// `"hash:<key>"`, where `<key>` names the routing key.
    Hash(String),
}

impl FromStr for Routing {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once(':') {
            None if s == "round-robin" => Ok(Self::RoundRobin),
            Some(("hash", key)) if !key.is_empty() => Ok(Self::Hash(key.to_string())),
            _ => Err(format!(
                "invalid routing '{s}' (expected \"round-robin\" or \"hash:<key>\")"
            )),
        }
    }
}

impl fmt::Display for Routing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RoundRobin => f.write_str("round-robin"),
            Self::Hash(key) => write!(f, "hash:{key}"),
        }
    }
}

impl TryFrom<String> for Routing {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Routing> for String {
    fn from(r: Routing) -> Self {
        r.to_string()
    }
}

/// The `instances` section: a fixed `count`, or a `min`/`max` range for
/// scaling (not supported in Phase 1).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Instances {
    /// Fixed number of instances. When nothing is set, one instance runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// Lower bound when scaling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<u32>,
    /// Upper bound when scaling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    /// How a pool picks an instance.
    pub routing: Routing,
}

impl Instances {
    /// The number of instances to start with.
    #[must_use]
    pub fn initial(&self) -> u32 {
        self.count.or(self.min).unwrap_or(1)
    }

    pub(crate) fn validate(&self, v: &mut Validator) {
        if self.count.is_some() && (self.min.is_some() || self.max.is_some()) {
            v.error_at("count", "set either count or min/max, not both");
        }
        if self.count == Some(0) {
            v.error_at("count", "must be at least 1");
        }
        if self.min == Some(0) {
            v.error_at("min", "must be at least 1");
        }
        if let (Some(min), Some(max)) = (self.min, self.max)
            && max < min
        {
            v.error_at("max", "must be at least min");
        }
    }
}

/// How an app moves to a new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeMode {
    /// Carry state across with `save-snapshot` and `load-snapshot`.
    Snapshot,
    /// Stop the old version gracefully and start the new one from `init`.
    #[default]
    Restart,
    /// Start new instances beside the old, move the registered name, drain
    /// the old. Not supported in Phase 1.
    BlueGreen,
}

/// The `upgrade` section.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Upgrade {
    /// Upgrade mode.
    pub mode: UpgradeMode,
    /// Version of the snapshot format this release writes. Required in
    /// `snapshot` mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_version: Option<u32>,
    /// App versions this release can upgrade from. Empty means any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub from_versions: Vec<VersionReq>,
}

impl Upgrade {
    pub(crate) fn validate(&self, v: &mut Validator) {
        if self.mode == UpgradeMode::Snapshot && self.snapshot_version.is_none() {
            v.error_at("snapshot_version", "is required when mode is \"snapshot\"");
        }
        if self.snapshot_version == Some(0) {
            v.error_at("snapshot_version", "must be at least 1");
        }
    }
}
