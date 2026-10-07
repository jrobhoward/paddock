//! Opening a bundle and verifying it against operator-trusted keys.

use std::collections::BTreeSet;
use std::path::Path;

use crate::manifest::{
    AppInfo, AppManifest, AppSpec, Capability, ComponentRef, Kind, ManifestError, Provenance,
    SECTION,
};
use rustbridge_bundle::{BundleLoader, Platform};

use crate::error::BundleError;
use crate::grant::{EffectiveGrant, effective_grant};
use crate::imports::component_imports;
use crate::policy::OperatorPolicy;

/// The WIT worlds this node can run.
pub const SUPPORTED_WORLDS: &[&str] = &["paddock:runtime/app@0.1.0"];

/// The variant Phase 1 deploys.
const VARIANT: &str = "release";

/// The archive directory whose files an app sees read-only at `/priv`.
pub const PRIV_DIR: &str = "priv/";

/// A bundle whose signature, checksums and manifest have been checked.
#[derive(Debug, Clone)]
pub struct VerifiedBundle {
    /// The app manifest.
    pub manifest: AppManifest,
    /// The component bytes, checksum- and signature-verified.
    pub component: Vec<u8>,
    /// The files under `priv/`, by path relative to it, each checked against
    /// the signed manifest's `files` checksums.
    pub priv_files: Vec<(String, Vec<u8>)>,
}

impl VerifiedBundle {
    /// Open a `.rbp` bundle and verify it.
    ///
    /// Checks, in order: the manifest signature against `trusted_keys` (the
    /// key inside the bundle is ignored), the `wasm32-wasip2` release
    /// variant's checksum and signature, the component's world, and the
    /// `appserver` section.
    ///
    /// # Errors
    ///
    /// [`BundleError::Untrusted`] if no trusted key signed the bundle,
    /// [`BundleError::Corrupt`] on a checksum mismatch, and the other
    /// [`BundleError`] variants for a bundle that is not a valid Paddock app.
    pub fn open(path: impl AsRef<Path>, trusted_keys: &[String]) -> Result<Self, BundleError> {
        let keys: Vec<&str> = trusted_keys.iter().map(String::as_str).collect();
        let mut loader = BundleLoader::open(path)?;

        let key_index = loader.verify_manifest_signature_trusted(&keys)?;
        let rb = loader.manifest().clone();
        let variant = rb
            .get_variant(Platform::Wasm32Wasip2, Some(VARIANT))
            .ok_or(BundleError::NoComponent)?;
        let component = loader.read_variant_verified(Platform::Wasm32Wasip2, VARIANT, &keys)?;
        let mut priv_files = Vec::new();
        for path in loader.list_files() {
            let Some(relative) = path.strip_prefix(PRIV_DIR) else {
                continue;
            };
            if relative.is_empty() || path.ends_with('/') {
                continue; // a directory entry
            }
            let bytes = loader.read_file_verified(&path)?;
            priv_files.push((relative.to_string(), bytes));
        }
        priv_files.sort();

        let world = variant
            .component
            .as_ref()
            .map(|c| c.world.clone())
            .unwrap_or_default();
        if !SUPPORTED_WORLDS.contains(&world.as_str()) {
            return Err(BundleError::UnsupportedWorld {
                world,
                supported: SUPPORTED_WORLDS.join(", "),
            });
        }

        let section = rb.extension(SECTION).ok_or(BundleError::NoAppSection)?;
        let spec = AppSpec::from_json_value(section).map_err(prefix_section)?;
        if spec.kind != Kind::WasmComponent {
            return Err(BundleError::UnsupportedKind(
                serde_json::to_value(spec.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default(),
            ));
        }

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
            component: ComponentRef {
                path: variant.library.clone(),
                checksum: variant.checksum.clone(),
                world,
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
            component,
            priv_files,
        })
    }

    /// Write the `priv/` files under `dir`, creating directories as needed.
    ///
    /// # Errors
    ///
    /// [`BundleError::Unreadable`] wrapping the I/O error.
    pub fn extract_priv(&self, dir: &Path) -> Result<(), BundleError> {
        let io = |e: std::io::Error| BundleError::Unreadable(rustbridge_bundle::BundleError::Io(e));
        std::fs::create_dir_all(dir).map_err(io)?;
        for (relative, bytes) in &self.priv_files {
            // rustbridge refuses absolute paths and `..` in the files map.
            let target = dir.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            std::fs::write(&target, bytes).map_err(io)?;
        }
        Ok(())
    }

    /// The component's top-level imports.
    ///
    /// # Errors
    ///
    /// [`BundleError::InvalidComponent`] if the bytes are not a component.
    pub fn imports(&self) -> Result<Vec<String>, BundleError> {
        component_imports(&self.component)
    }

    /// Check the component's imports against the manifest and compute the
    /// effective grant under `policy`.
    ///
    /// # Errors
    ///
    /// [`BundleError::InvalidComponent`] if the bytes are not a component,
    /// and [`BundleError::ImportsRefused`] if any import is undeclared or
    /// unknown.
    pub fn effective_grant(&self, policy: &OperatorPolicy) -> Result<EffectiveGrant, BundleError> {
        let granted: BTreeSet<Capability> = policy.granted(
            &self.manifest.app.name,
            &self.manifest.provenance.signer_key,
        );
        effective_grant(&self.imports()?, &self.manifest.spec, &granted)
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
