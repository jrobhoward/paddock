//! Opening a bundle and verifying it against operator-trusted keys.

use std::path::{Component, Path};

use crate::manifest::{
    AppInfo, AppManifest, AppSpec, ComponentRef, Kind, ManifestError, PlacementMode, Provenance,
    SECTION, ValidationError,
};
use rustbridge_bundle::{BundleLoader, Platform};

use crate::error::BundleError;
use crate::grant::{EffectiveGrant, effective_grant};
use crate::imports::component_imports;
use crate::policy::OperatorPolicy;

/// The world of an app: callbacks driven one message at a time.
pub const APP_WORLD: &str = "paddock:runtime/app@0.1.0";

/// The world of a connection handler: one instance per connection accepted
/// on the app's listeners.
pub const CONNECTION_WORLD: &str = "paddock:connection/connection-handler@0.1.0";

/// The world of an HTTP handler: an unmodified `wasi:http` component, one
/// instance per request on the app's routes. Any 0.2 version of
/// `wasi:http/incoming-handler` is this world.
pub const HTTP_HANDLER_WORLD: &str = "wasi:http/proxy@0.2.0";

/// The WIT worlds this node can run.
pub const SUPPORTED_WORLDS: &[&str] = &[APP_WORLD, CONNECTION_WORLD, HTTP_HANDLER_WORLD];

/// Refuse sections that make no sense for `world`: listeners outside a
/// connection handler, and for an HTTP handler (an instance per request)
/// no routes, group membership, or placement other than `any`. The error
/// is the field's path under the section, and why.
fn check_world(world: &str, spec: &AppSpec) -> Result<(), (&'static str, String)> {
    let connection = world == CONNECTION_WORLD;
    if connection == spec.listeners.is_empty() {
        return Err((
            "listeners",
            if connection {
                "a connection handler must declare at least one listener".into()
            } else {
                "only a connection handler (paddock:connection) declares listeners".into()
            },
        ));
    }
    if world != HTTP_HANDLER_WORLD {
        return Ok(());
    }
    let caps = &spec.capabilities;
    if caps
        .http_incoming
        .as_ref()
        .is_none_or(|s| s.routes.is_empty())
    {
        return Err((
            "capabilities",
            "an HTTP handler (wasi:http/incoming-handler) must declare at least one route \
             under wasi:http/incoming-handler"
                .into(),
        ));
    }
    if caps.groups.is_some() {
        return Err((
            "capabilities",
            "an HTTP handler lives for one request and cannot join or publish to groups \
             (paddock:groups)"
                .into(),
        ));
    }
    if spec
        .placement
        .as_ref()
        .is_some_and(|p| p.mode != PlacementMode::Any)
    {
        return Err((
            "placement",
            "an HTTP handler runs on every node it is deployed to; placement.mode must be `any`"
                .into(),
        ));
    }
    Ok(())
}

/// The variant Phase 1 deploys.
const VARIANT: &str = "release";

/// The archive directory whose files an app sees read-only at `/priv`.
pub const PRIV_DIR: &str = "priv/";

/// A bundle whose signature, checksums and manifest have been checked.
#[derive(Debug, Clone)]
pub struct VerifiedBundle {
    /// The app manifest.
    pub manifest: AppManifest,
    /// The component bytes, or for a `native-process` app its program for
    /// this node's platform; checksum- and signature-verified.
    pub component: Vec<u8>,
    /// The files under `priv/`, by path relative to it, each checked against
    /// the signed manifest's `files` checksums.
    pub priv_files: Vec<(String, Vec<u8>)>,
}

impl VerifiedBundle {
    /// Open a `.rbp` bundle and verify it.
    ///
    /// Checks, in order: the manifest signature against `trusted_keys` (the
    /// key inside the bundle is ignored), the `appserver` section, the
    /// release variant's checksum and signature (`wasm32-wasip2`, or for a
    /// `native-process` app this node's platform), and a component's world.
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
        let section = rb.extension(SECTION).ok_or(BundleError::NoAppSection)?;
        let spec = AppSpec::from_json_value(section).map_err(prefix_section)?;
        // A component for every node, or a native program for this one's
        // platform.
        let platform = if spec.kind == Kind::NativeProcess {
            Platform::current().ok_or_else(|| {
                BundleError::NoNativeVariant(format!(
                    "{}-{}",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ))
            })?
        } else {
            Platform::Wasm32Wasip2
        };
        let variant = rb
            .get_variant(platform, Some(VARIANT))
            .ok_or_else(|| match platform {
                Platform::Wasm32Wasip2 => BundleError::NoComponent,
                other => BundleError::NoNativeVariant(other.as_str().to_string()),
            })?;
        let component = loader.read_variant_verified(platform, VARIANT, &keys)?;
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

        // A native program has no world.
        let world = variant
            .component
            .as_ref()
            .map(|c| c.world.clone())
            .unwrap_or_default();
        if spec.kind != Kind::NativeProcess {
            if !SUPPORTED_WORLDS.contains(&world.as_str()) {
                return Err(BundleError::UnsupportedWorld {
                    world,
                    supported: SUPPORTED_WORLDS.join(", "),
                });
            }
            check_world(&world, &spec).map_err(|(path, message)| {
                ManifestError::Invalid(vec![ValidationError {
                    path: format!("{SECTION}.{path}"),
                    message,
                }])
            })?;
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

    /// The component's top-level imports; none for a native program.
    ///
    /// # Errors
    ///
    /// [`BundleError::InvalidComponent`] if the bytes are not a component.
    pub fn imports(&self) -> Result<Vec<String>, BundleError> {
        if self.manifest.spec.kind == Kind::NativeProcess {
            // A native program imports nothing: it asks the node over its
            // pipe, for what its manifest declares.
            return Ok(Vec::new());
        }
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
        let granted = policy.granted(
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
