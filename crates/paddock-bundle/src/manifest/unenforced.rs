//! Fields the data model accepts but the host does not enforce.
//!
//! The manifest model is complete so bundles written today stay valid later.
//! A field the node ignores could mislead an operator (a `placement.spread`
//! that does not spread), so the node logs a warning for each one a
//! manifest sets.

use std::fmt;

use serde::Serialize;

use crate::manifest::AppSpec;
use crate::manifest::sections::{NameScope, PlacementMode};

/// A field a manifest sets that the node does not enforce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnenforcedField {
    /// Dotted path of the field within the `appserver` section.
    pub path: String,
}

impl fmt::Display for UnenforcedField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not enforced", self.path)
    }
}

impl AppSpec {
    /// Fields this manifest sets that the host does not enforce, in manifest
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
        for (i, name) in self.registered_names.iter().enumerate() {
            if name.scope == NameScope::Global {
                out.add(format!("registered_names[{i}].scope"));
            }
        }
        out.add_if(self.instances.min.is_some(), "instances.min");
        out.add_if(self.instances.max.is_some(), "instances.max");
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
            out.add_if(set, format!("limits.{field}"));
        }
    }

    /// Whole capabilities the host does not enforce.
    fn unenforced_capabilities(&self, out: &mut Fields) {
        for capability in self.capabilities.declared() {
            if !capability.enforced() {
                out.add(format!("capabilities.{}", capability.key()));
            }
        }
    }

    /// `config`, `upgrade`, `placement` and `observability`. Of `placement`, a node with `[failover]` enforces
    /// `singleton` and `hot-standby`; the rest needs a scheduler.
    fn unenforced_sections(&self, out: &mut Fields) {
        out.add_if(!self.config.secrets.is_empty(), "config.secrets");
        if let Some(p) = &self.placement {
            out.add_if(p.mode == PlacementMode::AllNodes, "placement.mode");
            out.add_if(!p.required_labels.is_empty(), "placement.required_labels");
            out.add_if(!p.preferred_labels.is_empty(), "placement.preferred_labels");
            out.add_if(p.spread.is_some(), "placement.spread");
        }
        out.add_if(self.observability.metrics, "observability.metrics");
        out.add_if(
            self.observability.trace_sampling != 0.0,
            "observability.trace_sampling",
        );
    }
}

/// Accumulates unenforced fields.
struct Fields(Vec<UnenforcedField>);

impl Fields {
    fn add(&mut self, path: impl Into<String>) {
        self.0.push(UnenforcedField { path: path.into() });
    }

    fn add_if(&mut self, set: bool, path: impl Into<String>) {
        if set {
            self.add(path);
        }
    }
}
