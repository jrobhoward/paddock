//! Fields the data model accepts but Phase 1 does not enforce yet.
//!
//! The manifest model is complete so bundles written today stay valid later.
//! A field the node ignores could mislead an operator (a `placement.spread`
//! that does not spread), so the node logs a warning for each one a
//! manifest sets.

use std::fmt;

use serde::Serialize;

use crate::manifest::AppSpec;
use crate::manifest::capabilities::{Capability, Durability};
use crate::manifest::lifecycle::{Kind, UpgradeMode};
use crate::manifest::sections::{NameScope, PlacementMode};

/// When a field starts being enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Phase 2 (clustering).
    Phase2,
    /// Phase 3.
    Phase3,
    /// Not yet scheduled.
    Later,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Phase2 => "Phase 2",
            Self::Phase3 => "Phase 3",
            Self::Later => "a later phase",
        })
    }
}

/// A field a manifest sets that the node does not enforce yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnenforcedField {
    /// Dotted path of the field within the `appserver` section.
    pub path: String,
    /// When it will be enforced.
    pub phase: Phase,
}

impl fmt::Display for UnenforcedField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not enforced until {}", self.path, self.phase)
    }
}

impl AppSpec {
    /// Fields this manifest sets that Phase 1 does not enforce, in manifest
    /// order. Empty when every field set is enforced.
    #[must_use]
    pub fn unenforced_fields(&self) -> Vec<UnenforcedField> {
        let mut out = Fields(Vec::new());
        self.unenforced_runtime(&mut out);
        self.unenforced_capabilities(&mut out);
        self.unenforced_sections(&mut out);
        out.0
    }

    /// `kind`, `registered_names`, `instances` and `limits`.
    fn unenforced_runtime(&self, out: &mut Fields) {
        if self.kind == Kind::NativeProcess {
            out.add("kind", Phase::Later);
        }
        for (i, name) in self.registered_names.iter().enumerate() {
            if name.scope == NameScope::Global {
                out.add(format!("registered_names[{i}].scope"), Phase::Phase2);
            }
        }
        out.add_if(self.instances.min.is_some(), "instances.min", Phase::Later);
        out.add_if(self.instances.max.is_some(), "instances.max", Phase::Later);
        let limits = &self.limits;
        let later_limits = [
            ("fuel_per_message", limits.fuel_per_message.is_some()),
            ("storage_quota", limits.storage_quota.is_some()),
            ("open_files", limits.open_files.is_some()),
            (
                "outbound_connections",
                limits.outbound_connections.is_some(),
            ),
        ];
        for (field, set) in later_limits {
            out.add_if(set, format!("limits.{field}"), Phase::Later);
        }
    }

    /// Whole capabilities and the scope fields Phase 1 ignores.
    fn unenforced_capabilities(&self, out: &mut Fields) {
        for capability in self.capabilities.declared() {
            if let Some(phase) = capability.phase() {
                out.add(format!("capabilities.{}", capability.key()), phase);
            }
        }
        if let Some(kv) = &self.capabilities.keyvalue {
            for (name, bucket) in &kv.buckets {
                out.add_if(
                    bucket.durability == Durability::Replicated,
                    format!(
                        "capabilities.{}.buckets.{name}.durability",
                        Capability::KeyValue.key()
                    ),
                    Phase::Phase3,
                );
            }
        }
    }

    /// `config`, `upgrade`, `placement` and `observability`. Of `placement`, a node with `[failover]` enforces
    /// `singleton` and `hot-standby`; the rest needs a scheduler.
    fn unenforced_sections(&self, out: &mut Fields) {
        out.add_if(
            !self.config.secrets.is_empty(),
            "config.secrets",
            Phase::Phase2,
        );
        out.add_if(
            self.upgrade.mode == UpgradeMode::BlueGreen,
            "upgrade.mode",
            Phase::Later,
        );
        if let Some(p) = &self.placement {
            out.add_if(
                p.mode == PlacementMode::AllNodes,
                "placement.mode",
                Phase::Phase3,
            );
            out.add_if(
                !p.required_labels.is_empty(),
                "placement.required_labels",
                Phase::Phase3,
            );
            out.add_if(
                !p.preferred_labels.is_empty(),
                "placement.preferred_labels",
                Phase::Phase3,
            );
            out.add_if(p.spread.is_some(), "placement.spread", Phase::Phase3);
        }
        out.add_if(
            self.observability.metrics,
            "observability.metrics",
            Phase::Phase2,
        );
        out.add_if(
            self.observability.trace_sampling != 0.0,
            "observability.trace_sampling",
            Phase::Phase2,
        );
    }
}

/// Accumulates unenforced fields.
struct Fields(Vec<UnenforcedField>);

impl Fields {
    fn add(&mut self, path: impl Into<String>, phase: Phase) {
        self.0.push(UnenforcedField {
            path: path.into(),
            phase,
        });
    }

    fn add_if(&mut self, set: bool, path: impl Into<String>, phase: Phase) {
        if set {
            self.add(path, phase);
        }
    }
}
