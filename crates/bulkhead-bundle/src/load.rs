//! Opening a bundle and verifying it against operator-trusted keys.

use std::path::{Component, Path};

use crate::manifest::{
    AppInfo, AppManifest, AppSpec, ManifestError, ProgramRef, Provenance, SECTION,
};
use rustbridge_bundle::{BundleLoader, Platform};

use crate::error::BundleError;
use crate::grant::{EffectiveGrant, effective_grant};
use crate::policy::OperatorPolicy;
use crate::program::check_program;

/// The variant a node deploys.
const VARIANT: &str = "release";

/// The archive directory whose files an app sees read-only at `/priv`.
pub const PRIV_DIR: &str = "priv/";

/// A bundle whose signature, checksums and manifest have been checked.
#[derive(Debug, Clone)]
pub struct VerifiedBundle {
    /// The app manifest.
    pub manifest: AppManifest,
    /// The app's program for this node's platform; checksum- and
    /// signature-verified.
    pub program: Vec<u8>,
    /// The files under `priv/`, by path relative to it, each checked against
    /// the signed manifest's `files` checksums.
    pub priv_files: Vec<(String, Vec<u8>)>,
}

impl VerifiedBundle {
    /// Open a `.rbp` bundle and verify it.
    ///
    /// Checks, in order: the manifest signature against `trusted_keys` (the
    /// key inside the bundle is ignored), the `appserver` section, this
    /// node's platform's release variant's checksum and signature, and
    /// that the program is an ELF file for this node's architecture.
    ///
    /// # Errors
    ///
    /// [`BundleError::Untrusted`] if no trusted key signed the bundle,
    /// [`BundleError::Corrupt`] on a checksum mismatch,
    /// [`BundleError::Component`] for a bundle holding a WebAssembly
    /// component, and the other [`BundleError`] variants for a bundle that
    /// is not a valid Bulkhead app.
    pub fn open(path: impl AsRef<Path>, trusted_keys: &[String]) -> Result<Self, BundleError> {
        Self::open_as(path, trusted_keys, Platform::current())
    }

    /// Open and verify a bundle as a node on `platform` would: its program
    /// for that platform, for checking a bundle built for another machine.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn open_for_platform(
        path: impl AsRef<Path>,
        trusted_keys: &[String],
        platform: Platform,
    ) -> Result<Self, BundleError> {
        Self::open_as(path, trusted_keys, Some(platform))
    }

    fn open_as(
        path: impl AsRef<Path>,
        trusted_keys: &[String],
        native: Option<Platform>,
    ) -> Result<Self, BundleError> {
        let keys: Vec<&str> = trusted_keys.iter().map(String::as_str).collect();
        let mut loader = BundleLoader::open(path)?;

        let key_index = loader.verify_manifest_signature_trusted(&keys)?;
        let rb = loader.manifest().clone();
        let section = rb.extension(SECTION).ok_or(BundleError::NoAppSection)?;
        let spec = AppSpec::from_json_value(section).map_err(prefix_section)?;
        if rb.supported_platforms().contains(&Platform::Wasm32Wasip2) {
            return Err(BundleError::Component);
        }
        let no_native = |platform: String| {
            let present: Vec<&str> = rb
                .supported_platforms()
                .into_iter()
                .map(|p| p.as_str())
                .collect();
            BundleError::NoNativeVariant {
                platform,
                present: if present.is_empty() {
                    "none".to_string()
                } else {
                    present.join(", ")
                },
            }
        };
        let platform = native.ok_or_else(|| {
            no_native(format!(
                "{}-{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ))
        })?;
        let variant = rb
            .get_variant(platform, Some(VARIANT))
            .ok_or_else(|| no_native(platform.as_str().to_string()))?;
        let program = loader.read_variant_verified(platform, VARIANT, &keys)?;
        // A program filed under the wrong platform would fail only when
        // the runner executes it.
        check_program(platform, &program)?;
        let mut priv_files = Vec::new();
        for path in loader.list_files() {
            let Some(relative) = path.strip_prefix(PRIV_DIR) else {
                continue;
            };
            if relative.is_empty() || path.ends_with('/') {
                continue; // a directory entry
            }
            if !is_safe_priv_path(relative) {
                return Err(BundleError::UnsafePath(relative.to_string()));
            }
            let bytes = loader.read_file_verified(&path)?;
            priv_files.push((relative.to_string(), bytes));
        }
        priv_files.sort();

        let version =
            rb.plugin
                .version
                .parse()
                .map_err(|e: semver::Error| ManifestError::Parse {
                    path: "plugin.version".to_string(),
                    message: e.to_string(),
                })?;
        let manifest = AppManifest {
            app: AppInfo {
                name: rb.plugin.name.clone(),
                version,
                description: rb.plugin.description.clone(),
                authors: rb.plugin.authors.clone(),
                license: rb.plugin.license.clone(),
                repository: rb.plugin.repository.clone(),
            },
            program: ProgramRef {
                path: variant.library.clone(),
                checksum: variant.checksum.clone(),
            },
            provenance: Provenance {
                signer_key: trusted_keys[key_index].clone(),
                build_info: to_value(rb.build_info.as_ref()),
                sbom: to_value(rb.sbom.as_ref()),
            },
            spec,
        };
        manifest.validate()?;

        Ok(Self {
            manifest,
            program,
            priv_files,
        })
    }

    /// The app name and version a bundle's manifest names, read without
    /// verifying its signature or contents: for planning a rollout only,
    /// before a node opens and verifies it.
    ///
    /// # Errors
    ///
    /// The [`BundleError`] of a file that is not a bundle.
    pub fn unverified_identity(path: impl AsRef<Path>) -> Result<(String, String), BundleError> {
        let loader = BundleLoader::open(path)?;
        let rb = loader.manifest();
        Ok((rb.plugin.name.clone(), rb.plugin.version.clone()))
    }

    /// Write the `priv/` files under `dir`, creating directories as needed.
    ///
    /// # Errors
    ///
    /// [`BundleError::UnsafePath`] for a path that could leave `dir` (checked
    /// again here, since `priv_files` is public), [`BundleError::Unreadable`]
    /// wrapping an I/O error.
    pub fn extract_priv(&self, dir: &Path) -> Result<(), BundleError> {
        let io = |e: std::io::Error| BundleError::Unreadable(rustbridge_bundle::BundleError::Io(e));
        if let Some((bad, _)) = self.priv_files.iter().find(|(p, _)| !is_safe_priv_path(p)) {
            return Err(BundleError::UnsafePath(bad.clone()));
        }
        std::fs::create_dir_all(dir).map_err(io)?;
        for (relative, bytes) in &self.priv_files {
            let target = dir.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            std::fs::write(&target, bytes).map_err(io)?;
        }
        Ok(())
    }

    /// The effective grant under `policy`: what the manifest declares and
    /// the operator grants.
    #[must_use]
    pub fn effective_grant(&self, policy: &OperatorPolicy) -> EffectiveGrant {
        let granted = policy.granted(
            &self.manifest.app.name,
            &self.manifest.provenance.signer_key,
        );
        effective_grant(&self.manifest.spec, &granted)
    }
}

fn to_value<T: serde::Serialize>(value: Option<&T>) -> Option<serde_json::Value> {
    value.and_then(|v| serde_json::to_value(v).ok())
}

/// Root the paths in a section error at the bundle manifest.
fn prefix_section(err: ManifestError) -> ManifestError {
    let prefix = |path: String| {
        if path == "." {
            SECTION.to_string()
        } else {
            format!("{SECTION}.{path}")
        }
    };
    match err {
        ManifestError::Parse { path, message } => ManifestError::Parse {
            path: prefix(path),
            message,
        },
        ManifestError::Invalid(errors) => ManifestError::Invalid(
            errors
                .into_iter()
                .map(|mut e| {
                    e.path = prefix(e.path);
                    e
                })
                .collect(),
        ),
    }
}

/// Whether `relative`, a path under `priv/`, stays inside the directory it
/// is extracted into: only plain names separated by `/`, none of them `.`
/// or `..`, no root, and no `:` or backslash (a drive and a separator on
/// Windows).
/// rustbridge-bundle refuses such paths too; this check does not rely on it.
pub(crate) fn is_safe_priv_path(relative: &str) -> bool {
    !relative.is_empty()
        && !relative.contains('\\')
        && !relative.contains(':')
        && relative
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && Path::new(relative)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
#[path = "load_tests.rs"]
mod load_tests;
