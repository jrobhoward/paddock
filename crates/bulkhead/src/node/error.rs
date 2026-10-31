//! Node errors.

use thiserror::Error;

/// Why an admin operation failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NodeError {
    /// No such app or instance.
    #[error("{0}")]
    NotFound(String),
    /// The operation conflicts with the node's state (already deployed,
    /// still needed by another app, an upgrade in progress).
    #[error("{0}")]
    Conflict(String),
    /// The request or configuration is malformed.
    #[error("{0}")]
    Invalid(String),
    /// The bundle or upgrade was refused: untrusted, corrupt, undeclared
    /// imports, unapproved capabilities, or a rollout that reverted.
    #[error("{0}")]
    Refused(String),
    /// The node failed (storage, compilation).
    #[error("{0}")]
    Internal(String),
}

impl NodeError {
    /// The API error kind.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Invalid(_) => "invalid",
            Self::Refused(_) => "refused",
            Self::Internal(_) => "internal",
        }
    }

    /// The HTTP status for this error.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 409,
            Self::Invalid(_) => 400,
            Self::Refused(_) => 422,
            Self::Internal(_) => 500,
        }
    }
}

impl NodeError {
    /// The same error with `note` added to its message.
    pub(crate) fn noting(self, note: &str) -> Self {
        let add = |m: String| format!("{m}; {note}");
        match self {
            Self::NotFound(m) => Self::NotFound(add(m)),
            Self::Conflict(m) => Self::Conflict(add(m)),
            Self::Invalid(m) => Self::Invalid(add(m)),
            Self::Refused(m) => Self::Refused(add(m)),
            Self::Internal(m) => Self::Internal(add(m)),
        }
    }
}

pub(crate) fn internal(e: impl std::fmt::Display) -> NodeError {
    NodeError::Internal(e.to_string())
}
