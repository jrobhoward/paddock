//! The Paddock app manifest: the full data model, parsing with path-aware
//! errors, defaults and validation.
//!
//! A Paddock bundle is a rustbridge `.rbp` (format 1.2). Its `manifest.json`
//! already carries the app's identity (`plugin`), the component (the
//! `wasm32-wasip2` variant) and provenance (`build_info`, signatures); the
//! `appserver` section carries the rest, modelled here as [`AppSpec`].
//! [`AppManifest`] combines the four once a bundle has been verified.
//!
//! The model covers every section in the design, including fields enforced
//! only in later phases; [`AppSpec::unenforced_fields`] lists the ones a
//! manifest sets that Phase 1 ignores. See
//! <https://github.com/jrobhoward/paddock/blob/main/docs/DESIGN.md> (App
//! manifest and capability model).
//!
//! ```
//! use paddock_bundle::manifest::AppSpec;
//!
//! let spec = AppSpec::from_json_str(r#"{
//!     "registered_names": ["orders"],
//!     "limits": { "memory_max": "64MiB", "callback_deadline_ms": 2000 },
//!     "capabilities": { "wasi:logging": { "max_level": "info" } }
//! }"#)?;
//! assert_eq!(spec.limits.memory_max.bytes(), 64 * 1024 * 1024);
//! # Ok::<(), paddock_bundle::manifest::ManifestError>(())
//! ```

mod bytesize;
mod capabilities;
mod error;
mod lifecycle;
mod limits;
mod sections;
mod unenforced;
mod validate;

pub use bytesize::{ByteSize, ParseByteSizeError};
pub use capabilities::{
    AccessMode, AdminAccess, AdminScope, Bucket, Capabilities, Capability, ConfigScope, Durability,
    FilesystemScope, GroupsScope, HttpIncomingScope, HttpOutgoingScope, ImportClass, InterfaceName,
    KeyValueScope, LoggingScope, MetricsScope, PortRange, Preopen, RESERVED_NAMESPACES, Route,
    SecretsScope, SocketRule, SocketsScope, SqlScope, SupervisorScope, classify_import,
    is_extension_key, parse_cidr,
};
pub use error::{ManifestError, ValidationError};
pub use lifecycle::{
    Backoff, ChildSpec, Instances, Kind, Lifecycle, RestartType, Routing, Strategy, Supervision,
    Upgrade, UpgradeMode,
};
pub use limits::{Limits, WASM32_MEMORY_MAX};
pub use sections::{
    Api, ConfigSpec, Dependency, Health, ListenerDecl, LogLevel, MessagePattern, MessageSpec,
    MessagingAcl, NameScope, Observability, Placement, PlacementMode, RegisteredName,
};
pub use unenforced::{Phase, UnenforcedField};
pub use validate::{MAX_NAME_LEN, is_valid_name};

use serde::{Deserialize, Serialize};
use validate::Validator;

/// Name of the `.rbp` manifest extension that holds the [`AppSpec`].
pub const SECTION: &str = "appserver";

/// The `appserver` section of a bundle manifest: everything the server needs
/// to place, limit, wire and upgrade an app. Every section is optional and
/// has defaults; unknown fields are rejected.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppSpec {
    /// What the bundle runs.
    pub kind: Kind,
    /// Message catalog.
    pub api: Api,
    /// Apps that must be running first, in start order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Dependency>,
    /// Names the app may register; registering any other name is refused.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub registered_names: Vec<RegisteredName>,
    /// Start, stop and restart behaviour.
    pub lifecycle: Lifecycle,
    /// Root supervisor and, for supervisor apps, children.
    pub supervision: Supervision,
    /// How many instances run and how a pool routes.
    pub instances: Instances,
    /// Per-instance resource caps.
    pub limits: Limits,
    /// Host interfaces requested, with scopes.
    pub capabilities: Capabilities,
    /// Who may message the app, and whom it may message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messaging_acl: Option<MessagingAcl>,
    /// The app's configuration keys.
    pub config: ConfigSpec,
    /// How the app moves to a new version.
    pub upgrade: Upgrade,
    /// Where instances may run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    /// Health checking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    /// Logging, metrics and tracing defaults.
    pub observability: Observability,
    /// Listeners a connection-handler app serves
    /// (`paddock:connection/connection-handler`); an app world has none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub listeners: Vec<ListenerDecl>,
}

impl AppSpec {
    /// Parse and validate an `appserver` section from JSON text.
    ///
    /// # Errors
    ///
    /// [`ManifestError::Parse`] if the JSON is malformed or does not match
    /// the model, with the path of the offending field;
    /// [`ManifestError::Invalid`] listing every rule it breaks.
    pub fn from_json_str(json: &str) -> Result<Self, ManifestError> {
        let mut de = serde_json::Deserializer::from_str(json);
        let spec: Self = serde_path_to_error::deserialize(&mut de).map_err(parse_error)?;
        spec.validate()?;
        Ok(spec)
    }

    /// Parse and validate an `appserver` section from a JSON value (as read
    /// from a bundle manifest's extensions).
    ///
    /// # Errors
    ///
    /// As for [`AppSpec::from_json_str`].
    pub fn from_json_value(value: &serde_json::Value) -> Result<Self, ManifestError> {
        let spec: Self = serde_path_to_error::deserialize(value).map_err(parse_error)?;
        spec.validate()?;
        Ok(spec)
    }

    /// Check every rule that does not depend on the app's identity.
    ///
    /// # Errors
    ///
    /// [`ManifestError::Invalid`] listing every broken rule, with paths
    /// relative to the section.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut v = Validator::default();
        self.check(&mut v);
        v.finish()
    }

    fn check(&self, v: &mut Validator) {
        v.at("api", |v| self.api.validate(v));
        for (i, dep) in self.dependencies.iter().enumerate() {
            v.at(format!("dependencies[{i}]"), |v| v.name("app", &dep.app));
        }
        let mut deps = std::collections::BTreeSet::new();
        for (i, dep) in self.dependencies.iter().enumerate() {
            if !deps.insert(dep.app.as_str()) {
                v.error_at(
                    format!("dependencies[{i}].app"),
                    format!("'{}' is listed more than once", dep.app),
                );
            }
        }
        v.unique_names(
            "registered_names",
            self.registered_names.iter().map(|n| n.name.as_str()),
        );
        v.at("lifecycle", |v| self.lifecycle.validate(v));
        v.at("supervision", |v| self.supervision.validate(v));
        v.at("instances", |v| self.instances.validate(v));
        v.at("limits", |v| self.limits.validate(v));
        v.at("capabilities", |v| self.capabilities.validate(v));
        v.at("config", |v| self.config.validate(v));
        v.at("upgrade", |v| self.upgrade.validate(v));
        if let Some(placement) = &self.placement {
            v.at("placement", |v| placement.validate(&self.instances, v));
        }
        if let Some(health) = &self.health {
            v.at("health", |v| health.validate(v));
        }
        v.at("observability", |v| self.observability.validate(v));
        for (i, l) in self.listeners.iter().enumerate() {
            v.at(format!("listeners[{i}]"), |v| l.validate(v));
        }
        v.unique_names("listeners", self.listeners.iter().map(|l| l.name.as_str()));
    }
}

fn parse_error(err: serde_path_to_error::Error<serde_json::Error>) -> ManifestError {
    ManifestError::Parse {
        path: err.path().to_string(),
        message: err.into_inner().to_string(),
    }
}

/// App identity, from the bundle's `plugin` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    /// App name; also its default registered name and storage namespace.
    pub name: String,
    /// App version.
    pub version: semver::Version,
    /// Short description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Authors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    /// License identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Repository URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// The component, from the bundle's `wasm32-wasip2` variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentRef {
    /// Path of the component within the bundle.
    pub path: String,
    /// `sha256:<hex>` checksum of the component.
    pub checksum: String,
    /// WIT world the component targets (`paddock:runtime/app@0.1.0`).
    pub world: String,
}

/// Where the bundle came from and who vouched for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The operator-trusted minisign public key that verified the bundle.
    pub signer_key: String,
    /// The bundle's `build_info` (builder, compiler, git commit), as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_info: Option<serde_json::Value>,
    /// The bundle's SBOM paths, as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sbom: Option<serde_json::Value>,
}

/// A verified app: identity, component, provenance and the `appserver`
/// section together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppManifest {
    /// Identity.
    pub app: AppInfo,
    /// The component.
    pub component: ComponentRef,
    /// Provenance.
    pub provenance: Provenance,
    /// The `appserver` section.
    pub spec: AppSpec,
}

impl AppManifest {
    /// Check every rule, including those that depend on the app's identity.
    /// Paths are rooted at the bundle manifest (`plugin.name`,
    /// `appserver.limits.memory_max`).
    ///
    /// # Errors
    ///
    /// [`ManifestError::Invalid`] listing every broken rule.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut v = Validator::default();
        v.at("plugin", |v| v.name("name", &self.app.name));
        v.at(SECTION, |v| {
            self.spec.check(v);
            for (i, dep) in self.spec.dependencies.iter().enumerate() {
                if dep.app == self.app.name {
                    v.error_at(
                        format!("dependencies[{i}].app"),
                        "an app cannot depend on itself",
                    );
                }
            }
        });
        v.finish()
    }
}

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod manifest_tests;
