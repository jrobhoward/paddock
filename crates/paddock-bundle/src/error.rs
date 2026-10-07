//! Errors from loading and checking a bundle.

use paddock_manifest::ManifestError;
use thiserror::Error;

/// Why a bundle was refused.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BundleError {
    /// The bundle is not signed by any operator-trusted key, is unsigned, or
    /// no trusted keys were configured.
    #[error("bundle is not trusted: {0}")]
    Untrusted(String),

    /// A file's contents do not match the checksum in the manifest.
    #[error("bundle is corrupt: {0}")]
    Corrupt(String),

    /// The bundle could not be read as a rustbridge `.rbp`.
    #[error("cannot read bundle: {0}")]
    Unreadable(#[source] rustbridge_bundle::BundleError),

    /// The bundle has no `wasm32-wasip2` release variant.
    #[error("bundle has no wasm32-wasip2 release variant")]
    NoComponent,

    /// The component targets a world this node does not implement.
    #[error("unsupported world '{world}' (this node supports {supported})")]
    UnsupportedWorld {
        /// The world the bundle declares; empty if it declares none.
        world: String,
        /// The worlds this node supports, comma-separated.
        supported: String,
    },

    /// The bundle's `kind` is not supported yet.
    #[error("app kind '{0}' is not supported in Phase 1")]
    UnsupportedKind(String),

    /// The bundle has no `appserver` section.
    #[error("bundle manifest has no '{}' section", paddock_manifest::SECTION)]
    NoAppSection,

    /// The app manifest is malformed or invalid.
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    /// The component bytes are not a valid WebAssembly component.
    #[error("invalid component: {0}")]
    InvalidComponent(String),

    /// The component imports interfaces the manifest does not declare, or
    /// that no host provides.
    #[error("{}", refused_message(.undeclared, .unknown))]
    ImportsRefused {
        /// Imports whose capability is not declared under `capabilities`.
        undeclared: Vec<String>,
        /// Imports no host provides.
        unknown: Vec<String>,
    },

    /// The operator policy file could not be read or parsed.
    #[error("invalid operator policy: {0}")]
    Policy(String),
}

fn refused_message(undeclared: &[String], unknown: &[String]) -> String {
    let mut parts = Vec::new();
    if !undeclared.is_empty() {
        parts.push(format!(
            "imports not declared under capabilities: {}",
            undeclared.join(", ")
        ));
    }
    if !unknown.is_empty() {
        parts.push(format!("imports no host provides: {}", unknown.join(", ")));
    }
    format!("deploy refused: {}", parts.join("; "))
}

impl From<rustbridge_bundle::BundleError> for BundleError {
    fn from(err: rustbridge_bundle::BundleError) -> Self {
        use rustbridge_bundle::BundleError as Rb;
        match err {
            Rb::SignatureVerificationFailed(msg) => Self::Untrusted(msg),
            Rb::NoPublicKey => Self::Untrusted("no trusted keys are configured".to_string()),
            Rb::ChecksumMismatch { .. } => Self::Corrupt(err.to_string()),
            Rb::Unverified(path) => {
                Self::Untrusted(format!("{path} is not covered by the signed manifest"))
            }
            other => Self::Unreadable(other),
        }
    }
}
