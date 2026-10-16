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
//! | `POST /v1/instances/{pid}/call?timeout_ms=N`, `/v1/names/{name}/call?timeout_ms=N` | as above, to one instance by pid or registered name | reply body |
//! | `POST /v1/instances/{pid}/cast`, `/v1/names/{name}/cast` | message body, with its `Content-Type` | [`Done`] |
//! | `GET /v1/apps/{app}/logs?lines=N` | | `Vec<LogLine>` |
//! | `GET /v1/apps/{app}/logs?lines=N&follow=true` | | a stream of [`LogLine`]s: the last N, then each new one |
//! | `GET /v1/events` | | a stream of [`Event`]s from now on |
//! | `POST /v1/apps/{app}/cast` | message body, with its `Content-Type` | [`Done`] |
//! | `POST /v1/apps/{app}/log-level?level=debug` (or `reset`) | | [`Done`] |
//! | `GET /v1/instances` | | `Vec<InstanceInfo>` |
//! | `GET /v1/instances/{pid}/state` | | [`StateResponse`] |
//! | `POST /v1/instances/{pid}/kill` | | [`Done`] |
//!
//! A stream is a chunked response of JSON lines (`application/x-ndjson`),
//! one value per line, with an empty line every 15 seconds when there is
//! nothing else; it ends when the node shuts down.
//! [`Client::follow_logs`] and [`Client::events`] read them.
//!
//! ```sh
//! curl --unix-socket /var/lib/paddock/admin.sock http://paddock/v1/apps
//! ```

mod client;

pub use client::{Client, ClientError, Follow};

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
    /// The app's listeners, for a connection handler.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listeners: Vec<ListenerInfo>,
}

/// One listener of a connection-handler app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerInfo {
    /// The listener's name in the manifest.
    pub name: String,
    /// The address it listens on.
    pub address: String,
    /// Connections open now.
    pub active: usize,
    /// Connections accepted in total.
    pub accepted: u64,
    /// Connections refused because `max_connections` were open.
    pub refused: u64,
    /// Connections whose handler failed.
    pub crashed: u64,
    /// Connections closed by the idle timeout.
    pub idle_closed: u64,
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
    /// Bytes of linear memory it has grown to.
    #[serde(default)]
    pub memory_bytes: u64,
    /// Times it has been restarted (its incarnation minus one).
    #[serde(default)]
    pub restarts: u32,
    /// Why its previous incarnation exited, if its supervisor restarted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<String>,
    /// Pids it is linked to, on any node.
    #[serde(default)]
    pub links: Vec<String>,
    /// Monitors it holds on others.
    #[serde(default)]
    pub monitoring: Vec<MonitorInfo>,
    /// Monitors others hold on it.
    #[serde(default)]
    pub monitored_by: Vec<MonitorInfo>,
}

/// One monitor in [`InstanceInfo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// The monitor's reference.
    pub reference: u64,
    /// The other end: the target for `monitoring`, the watcher for
    /// `monitored_by`.
    pub pid: String,
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

/// One member of the cluster, as the node asked knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    /// Its node id.
    pub node: u32,
    /// Its creation (raised on every start).
    pub creation: u32,
    /// Where it is reached.
    pub address: String,
    /// `member` or `witness`.
    pub role: String,
    /// Whether frames to it are sent (it is heard, or is this node).
    pub reachable: bool,
}

/// A node's failover in its HA pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailoverInfo {
    /// The other node of the pair.
    pub partner: u32,
    /// Who promotes the standby: `witness <node>`, `paths` or `operator`.
    pub arbiter: String,
    /// `primary`, `standby` or `down`.
    pub role: String,
    /// The node's epoch, raised by every promotion.
    pub epoch: u64,
    /// Whether singletons run here now.
    pub may_run: bool,
    /// The partner looks gone and only `paddock failover confirm` would
    /// promote this node.
    pub awaiting_confirmation: bool,
    /// Whether the partner is heard over the cluster transport.
    pub partner_reachable: bool,
    /// The singleton apps this node holds, in deploy order.
    pub singletons: Vec<String>,
    /// Raised by every change of the node's leadership (role, epoch,
    /// whether singletons may run, awaiting a confirmation); pass it back
    /// as `after` to wait for the next ([`crate::api::Client::failover_after`]).
    #[serde(default)]
    pub generation: u64,
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
    /// `snapshot`, `restart`, or `new-connections` for a connection handler
    /// (new connections get the new version; open ones keep theirs).
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

/// Where a call or cast goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An app, through its pool's routing.
    App(String),
    /// One instance, by pid (`node.serial.incarnation@creation`, as `ps` shows it).
    Pid(String),
    /// The instance holding a registered name.
    Name(String),
}

impl Target {
    /// The route for `verb` (`call` or `cast`) to this target.
    #[must_use]
    pub fn path(&self, verb: &str) -> String {
        match self {
            Self::App(app) => format!("/v1/apps/{app}/{verb}"),
            Self::Pid(pid) => format!("/v1/instances/{pid}/{verb}"),
            Self::Name(name) => format!("/v1/names/{name}/{verb}"),
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::App(app) => f.write_str(app),
            Self::Pid(pid) => write!(f, "pid {pid}"),
            Self::Name(name) => write!(f, "name {name}"),
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

/// One record of the node's event stream (`GET /v1/events`).
///
/// `kind` and `event` say what happened:
///
/// | `kind` | `event` | Fields |
/// | --- | --- | --- |
/// | `audit` | as the audit log (`deploy`, `upgrade`, `stop`, `remote_admin`, ...) | as the audit log |
/// | `instance` | `exit` | `pid`, `app`, `reason` |
/// | `instance` | `restart` | `pid`, `previous`, `child` |
/// | `member` | `joined`, `left`, `reachable`, `unreachable`, `down`, `path_silent`, `path_heard` | `node`, and `creation`, `session` or `path` |
/// | `failover` | `leadership` | `role`, `epoch`, `may_run`, `awaiting_confirmation`, `generation` |
/// | `node` | `lagged` | `missed`: events this follower lost by falling behind |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Milliseconds since the Unix epoch.
    pub time_ms: u64,
    /// What it is about.
    pub kind: String,
    /// What happened.
    pub event: String,
    /// The rest of its fields.
    #[serde(flatten)]
    pub fields: serde_json::Map<String, serde_json::Value>,
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
