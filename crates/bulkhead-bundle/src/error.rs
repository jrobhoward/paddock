//! Errors from loading and checking a bundle.

use crate::manifest::ManifestError;
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

    /// A `native-process` bundle has no release variant for this node's
    /// platform.
    #[error(
        "bundle has no program for {platform}, this node's platform (it has {present}); \
         `bulkhead bundle --program {platform}=PATH` adds one"
    )]
    NoNativeVariant {
        /// This node's platform (`linux-aarch64`).
        platform: String,
        /// The native platforms the bundle has programs for,
        /// comma-separated, or `none`.
        present: String,
    },

    /// A native program is not an ELF file for its platform's
    /// architecture.
    #[error("the program for {platform} is {found}")]
    WrongProgram {
        /// The platform the program is filed under.
        platform: String,
        /// What the program is instead (`an ELF file for x86_64`).
        found: String,
    },

    /// A native program for a platform `native-process` apps do not run
    /// on.
    #[error("native-process apps run on linux-x86_64 and linux-aarch64, not {0}")]
    NotNativePlatform(String),

    /// The bundle has no `appserver` section.
    #[error("bundle manifest has no '{}' section", crate::manifest::SECTION)]
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

    /// A file under `priv/` has a path that could leave the directory it is
    /// extracted into: absolute, with `..`, or not a plain relative path.
    #[error("bundle file 'priv/{0}' has an unsafe path (absolute, '..' or a drive)")]
    UnsafePath(String),

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
