//! The Paddock admin API: JSON over HTTP/1.1 on the node's Unix socket.
//!
//! Every path is under `/v1`. Errors come back as [`ApiError`] bodies with a
//! 4xx or 5xx status. The socket is created with mode 0600; access to it is
//! the authorization.
//!
//! | Method and path | Body | Returns |
//! | --- | --- | --- |
//! | `GET /v1/apps` | | `Vec<AppSummary>` |
//! | `POST /v1/apps` | `.rbp` bytes | [`DeployResponse`] |
//! | `GET /v1/apps/{app}` | | [`AppDetail`] |
//! | `DELETE /v1/apps/{app}` | | [`Done`] |
//! | `POST /v1/apps/{app}/stop`, `/start` | | [`Done`] |
//! | `POST /v1/apps/{app}/upgrade?approve=true&batch=N&observe_ms=N&max_restarts=N&restart=true` | `.rbp` bytes | [`UpgradeResponse`] |
//! | `POST /v1/apps/{app}/rollback?batch=N&observe_ms=N&max_restarts=N&restart=true` | | [`UpgradeResponse`] |
//! | `POST /v1/apps/{app}/call?timeout_ms=N` | message body, with its `Content-Type` | reply body |
//! | `GET /v1/apps/{app}/logs?lines=N` | | `Vec<LogLine>` |
//! | `POST /v1/apps/{app}/cast` | message body, with its `Content-Type` | [`Done`] |
//! | `POST /v1/apps/{app}/log-level?level=debug` (or `reset`) | | [`Done`] |
//! | `GET /v1/instances` | | `Vec<InstanceInfo>` |
//! | `GET /v1/instances/{pid}/state` | | [`StateResponse`] |
//! | `POST /v1/instances/{pid}/kill` | | [`Done`] |
//!
//! ```sh
//! curl --unix-socket /var/lib/paddock/admin.sock http://paddock/v1/apps
//! ```

mod client;

pub use client::{Client, ClientError};

use serde::{Deserialize, Serialize};

/// One app, as `GET /v1/apps` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSummary {
    /// App name.
    pub name: String,
    /// Running (or last deployed) version.
    pub version: String,
    /// `starting`, `running`, `failed`, `stopped`.
    pub status: String,
    /// Why it failed, for `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Running instances.
    pub instances: usize,
    /// The version a rollback returns to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

/// One app in detail, as `GET /v1/apps/{app}` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppDetail {
    /// The summary.
    #[serde(flatten)]
    pub summary: AppSummary,
    /// The operator key that verified the bundle.
    pub signer: String,
    /// Capabilities the app runs with.
    pub granted: Vec<String>,
    /// Capabilities declared but not granted (linked to deny stubs).
    pub denied: Vec<String>,
    /// Imported interfaces no host provides (deny stubs).
    pub stubbed: Vec<String>,
    /// Manifest fields this node does not enforce yet.
    pub warnings: Vec<String>,
    /// Restarts by the app's root supervisor since it started.
    pub restarts: u64,
    /// The running instances.
    pub instance_list: Vec<InstanceInfo>,
}

/// One running instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// `node.id.incarnation`.
    pub pid: String,
    /// The app it runs.
    pub app: String,
    /// The version it runs.
    pub version: String,
    /// Lifecycle state (`active`, `upgrading`, ...).
    pub state: String,
    /// Messages waiting in its mailbox.
    pub queued: usize,
    /// Casts and timer messages dropped because its mailbox was full.
    #[serde(default)]
    pub dropped: u64,
}

/// An instance's state, from its `save-snapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateResponse {
    /// `node.id.incarnation`.
    pub pid: String,
    /// Snapshot size in bytes.
    pub bytes: usize,
    /// How it was read: `paddock-sdk` (a `[format_version, state]` CBOR
    /// pair), `json`, `text`, `cbor`, `raw` or `empty`.
    pub format: String,
    /// The SDK snapshot's format version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_version: Option<u64>,
    /// The decoded state, for every format but `raw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// The bytes in hex, for `raw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hex: Option<String>,
}

/// What a deploy did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployResponse {
    /// The app, its version and status.
    pub app: AppSummary,
    /// Capabilities granted.
    pub granted: Vec<String>,
    /// Capabilities declared but not granted.
    pub denied: Vec<String>,
    /// Manifest fields this node does not enforce yet.
    pub warnings: Vec<String>,
}

/// What an upgrade or rollback did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeResponse {
    /// Version before.
    pub from: String,
    /// Version after.
    pub to: String,
    /// `snapshot` or `restart`.
    pub mode: String,
    /// Pids moved.
    pub instances: Vec<String>,
    /// Pause per instance, in microseconds (`snapshot` mode).
    pub pauses_us: Vec<u64>,
    /// Capabilities approved for the new version.
    pub approved: Vec<String>,
}

/// Rollout settings for upgrade and rollback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutParams {
    /// Approve capabilities the new version adds.
    pub approve: bool,
    /// Instances per batch (default 1).
    pub batch: Option<usize>,
    /// Watch time per batch, in milliseconds.
    pub observe_ms: Option<u64>,
    /// Restarts tolerated during the rollout (default 0).
    pub max_restarts: Option<u64>,
    /// Move instances by restart, whatever the manifest's upgrade mode:
    /// each starts the other version from `init`, and in-memory state is
    /// lost.
    #[serde(default)]
    pub restart: bool,
}

impl RolloutParams {
    /// The query string for these settings, starting with `?` (or empty).
    #[must_use]
    pub fn query(&self) -> String {
        let mut parts = Vec::new();
        if self.approve {
            parts.push("approve=true".to_string());
        }
        if let Some(b) = self.batch {
            parts.push(format!("batch={b}"));
        }
        if let Some(o) = self.observe_ms {
            parts.push(format!("observe_ms={o}"));
        }
        if let Some(m) = self.max_restarts {
            parts.push(format!("max_restarts={m}"));
        }
        if self.restart {
            parts.push("restart=true".to_string());
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("?{}", parts.join("&"))
        }
    }
}

/// One log record from an app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    /// Milliseconds since the Unix epoch.
    pub time_ms: u64,
    /// `TRACE` to `ERROR`.
    pub level: String,
    /// The instance's pid.
    pub pid: String,
    /// `stdout`, `stderr`, or the `wasi:logging` context.
    pub source: String,
    /// The text.
    pub message: String,
}

/// An operation that succeeded with nothing else to say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    /// What happened, for people.
    pub message: String,
}

/// An error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// A stable category: `not_found`, `conflict`, `invalid`, `refused`,
    /// `internal`.
    pub kind: String,
    /// What went wrong, for people.
    pub error: String,
}
