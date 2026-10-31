//! Errors from parsing and validating a manifest.

use std::fmt;

use thiserror::Error;

/// A manifest that could not be parsed or failed validation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// The manifest is not well formed: bad syntax, a wrong type, a missing
    /// required field or an unknown field.
    #[error("{path}: {message}")]
    Parse {
        /// Where the problem is, as a dotted path (`limits.memory_max`); `.`
        /// for the root.
        path: String,
        /// What is wrong.
        message: String,
    },

    /// The manifest parsed but breaks one or more rules. Every broken rule is
    /// listed, not only the first.
    #[error("invalid manifest: {}", join(.0))]
    Invalid(Vec<ValidationError>),
}

/// One broken rule in a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// Where the problem is, as a dotted path
    /// (`capabilities.wasi:keyvalue.buckets.orders.quota`).
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

fn join(errors: &[ValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}
