//! The `limits` section: per-instance resource caps.

use serde::{Deserialize, Serialize};

use crate::manifest::bytesize::ByteSize;
use crate::manifest::validate::Validator;

/// The `limits` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Most linear memory one instance may grow to. Growth past it fails and
    /// the instance crashes with `memory_limit`.
    pub memory_max: ByteSize,
    /// Most guest execution time one callback may take before the instance is
    /// killed with reason `timeout`. Time spent in host calls (waiting on
    /// another instance) does not count.
    pub callback_deadline_ms: u64,
    /// Most messages waiting in one instance's mailbox. When full, casts are
    /// dropped and calls fail with `overloaded`.
    pub mailbox_max: u32,
    /// Most bytes of message bodies waiting in one instance's mailbox
    /// (casts, calls and timer messages). A message that would go past it
    /// is treated as at a full mailbox. At least `message_max_bytes`, so
    /// every message fits an empty mailbox.
    pub mailbox_max_bytes: ByteSize,
    /// Largest message body accepted.
    pub message_max_bytes: ByteSize,
    /// Total storage quota across the app's buckets and volumes. Not
    /// enforced; per-bucket quotas under `capabilities` are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_quota: Option<ByteSize>,
    /// Most open files. Not enforced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_files: Option<u32>,
    /// Most concurrent outbound connections. Not enforced (`bulkhead:sockets`
    /// has `max_sockets`, HTTP egress `max_concurrent`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbound_connections: Option<u32>,
    /// Most pending `send-after` timers per instance. Unset uses the node's
    /// value, which is also the most a manifest may ask for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timers_max: Option<u32>,
    /// Most monitors one instance holds on others. Unset uses the node's
    /// value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitors_max: Option<u32>,
    /// Most links one instance may hold when it links again. Unset uses the
    /// node's value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links_max: Option<u32>,
    /// Most calls one instance holds unanswered after `noreply`. Unset uses
    /// the node's value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_calls_max: Option<u32>,
    /// Most process groups one instance is in (`bulkhead:groups`). Unset
    /// uses the node's value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups_max: Option<u32>,
    /// Most keyvalue watches one instance holds (`bulkhead:watch`). Unset
    /// uses the node's value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watches_max: Option<u32>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_max: ByteSize::mib(64),
            callback_deadline_ms: 5_000,
            mailbox_max: 10_000,
            mailbox_max_bytes: ByteSize::mib(64),
            message_max_bytes: ByteSize::mib(1),
            storage_quota: None,
            open_files: None,
            outbound_connections: None,
            timers_max: None,
            monitors_max: None,
            links_max: None,
            pending_calls_max: None,
            groups_max: None,
            watches_max: None,
        }
    }
}

impl Limits {
    pub(crate) fn validate(&self, v: &mut Validator) {
        if self.memory_max.bytes() == 0 {
            v.error_at("memory_max", "must be greater than zero");
        }
        if self.callback_deadline_ms == 0 {
            v.error_at("callback_deadline_ms", "must be greater than zero");
        }
        if self.mailbox_max == 0 {
            v.error_at("mailbox_max", "must be greater than zero");
        }
        if self.message_max_bytes.bytes() == 0 {
            v.error_at("message_max_bytes", "must be greater than zero");
        }
        if self.mailbox_max_bytes < self.message_max_bytes {
            v.error_at(
                "mailbox_max_bytes",
                format!(
                    "must be at least message_max_bytes ({}), so every message fits an empty mailbox",
                    self.message_max_bytes
                ),
            );
        }
        for (field, value) in [
            ("timers_max", self.timers_max.map(u64::from)),
            ("monitors_max", self.monitors_max.map(u64::from)),
            ("links_max", self.links_max.map(u64::from)),
            ("pending_calls_max", self.pending_calls_max.map(u64::from)),
            ("groups_max", self.groups_max.map(u64::from)),
            ("watches_max", self.watches_max.map(u64::from)),
        ] {
            if value == Some(0) {
                v.error_at(field, "must be greater than zero");
            }
        }
    }
}
