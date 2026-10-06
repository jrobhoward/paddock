//! What one instance runs with.

use std::time::Duration;

use paddock_manifest::AppManifest;

/// Limits, timeouts and `init` arguments for one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    /// App name, for log records.
    pub app: String,
    /// App version, for log records.
    pub version: String,
    /// Most linear memory, in bytes.
    pub memory_max: u64,
    /// Most table elements. `None` leaves the engine default.
    pub table_max: Option<u32>,
    /// Guest execution time allowed per `handle-*` callback.
    pub callback_deadline: Duration,
    /// Mailbox capacity.
    pub mailbox_max: usize,
    /// Largest message body the instance accepts, in bytes.
    pub message_max_bytes: u64,
    /// Names the instance may register.
    pub registered_names: Vec<String>,
    /// Time allowed for `init`.
    pub init_timeout: Duration,
    /// Time allowed for `terminate` before the instance is killed.
    pub shutdown_timeout: Duration,
    /// Passed to `init`.
    pub init_args: Vec<u8>,
}

impl InstanceSpec {
    /// The spec a verified manifest describes. `lifecycle.start_args` is
    /// passed to `init` as JSON; absent args are an empty list. Every name in
    /// `registered_names` may be registered on this node; global scope is not
    /// enforced until Phase 2.
    #[must_use]
    pub fn from_manifest(manifest: &AppManifest) -> Self {
        let spec = &manifest.spec;
        Self {
            app: manifest.app.name.clone(),
            version: manifest.app.version.to_string(),
            memory_max: spec.limits.memory_max.bytes(),
            table_max: spec.limits.table_max,
            callback_deadline: Duration::from_millis(spec.limits.callback_deadline_ms),
            mailbox_max: usize::try_from(spec.limits.mailbox_max).unwrap_or(usize::MAX),
            message_max_bytes: spec.limits.message_max_bytes.bytes(),
            registered_names: spec
                .registered_names
                .iter()
                .map(|n| n.name.clone())
                .collect(),
            init_timeout: Duration::from_millis(spec.lifecycle.init_timeout_ms),
            shutdown_timeout: Duration::from_millis(spec.lifecycle.shutdown_timeout_ms),
            init_args: spec
                .lifecycle
                .start_args
                .as_ref()
                .map(|v| v.to_string().into_bytes())
                .unwrap_or_default(),
        }
    }
}
