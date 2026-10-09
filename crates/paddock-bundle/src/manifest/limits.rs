//! The `limits` section: per-instance resource caps.

use serde::{Deserialize, Serialize};

use crate::manifest::bytesize::ByteSize;
use crate::manifest::validate::Validator;

/// Largest linear memory a wasm32 instance can address.
pub const WASM32_MEMORY_MAX: ByteSize = ByteSize::gib(4);

/// The `limits` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Most linear memory one instance may grow to. Growth past it fails and
    /// the instance crashes with `memory_limit`.
    pub memory_max: ByteSize,
    /// Most table elements one instance may grow to. Unset uses the engine
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table_max: Option<u32>,
    /// Most guest execution time one callback may take before the instance is
    /// killed with reason `timeout`. Time spent in host calls (waiting on
    /// another instance) does not count.
    pub callback_deadline_ms: u64,
    /// Fuel budget per message, for deterministic metering. Not enforced in
    /// Phase 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuel_per_message: Option<u64>,
    /// Most messages waiting in one instance's mailbox. When full, casts are
    /// dropped and calls fail with `overloaded`.
    pub mailbox_max: u32,
    /// Largest message body accepted.
    pub message_max_bytes: ByteSize,
    /// Total storage quota across the app's buckets and volumes. Not enforced
    /// in Phase 1; per-bucket quotas under `capabilities` are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_quota: Option<ByteSize>,
    /// Most open files. Not enforced in Phase 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_files: Option<u32>,
    /// Most concurrent outbound connections. Not enforced in Phase 1.
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
    /// Most process groups one instance is in (`paddock:groups`). Unset
    /// uses the node's value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups_max: Option<u32>,
    /// Most keyvalue watches one instance holds (`paddock:watch`). Unset
    /// uses the node's value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watches_max: Option<u32>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_max: ByteSize::mib(64),
            table_max: None,
            callback_deadline_ms: 5_000,
            fuel_per_message: None,
            mailbox_max: 10_000,
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
        } else if self.memory_max > WASM32_MEMORY_MAX {
            v.error_at(
                "memory_max",
                format!("must be at most {WASM32_MEMORY_MAX} (the wasm32 address space)"),
            );
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
        for (field, value) in [
            ("fuel_per_message", self.fuel_per_message),
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
