//! The `capabilities` section, and how component imports map to it.
//!
//! A component can call only what it imports, so its imports are the upper
//! bound of its permissions. Each import falls in one of three classes
//! ([`classify_import`]):
//!
//! - Implicit: granted to every app and narrowed by host configuration, not
//!   declared. These are `paddock:runtime/types` and `paddock:runtime/process`
//!   (narrowed by `messaging_acl`), `wasi:clocks` and `wasi:random`, and the
//!   WASI plumbing Rust's standard library pulls in: `wasi:io` and `wasi:cli`
//!   (stdout and stderr become tagged logs, the environment holds only what the
//!   host sets).
//! - A [`Capability`]: must be declared under `capabilities`, keyed by
//!   [`Capability::key`], or the deploy is refused.
//! - An extension: an interface of a package outside the reserved `wasi` and
//!   `paddock` namespaces, served by a host service the node was built with.
//!   It must be declared under `capabilities.extensions`, keyed by its
//!   package (`"example:greeting"`), or the deploy is refused.
//! - Unknown: a `wasi` or `paddock` interface no host provides, so the
//!   deploy is refused.

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::manifest::bytesize::ByteSize;
use crate::manifest::sections::LogLevel;
use crate::manifest::unenforced::Phase;
use crate::manifest::validate::Validator;

/// A host interface an app must declare before importing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "&'static str", try_from = "String")]
#[non_exhaustive]
pub enum Capability {
    /// `paddock:runtime/supervisor`: start and supervise child instances.
    Supervisor,
    /// `wasi:logging`.
    Logging,
    /// `wasi:config`.
    Config,
    /// `paddock:secrets`.
    Secrets,
    /// `wasi:keyvalue`: term storage.
    KeyValue,
    /// `wasi:filesystem`: preopened volumes.
    Filesystem,
    /// `wasi:http/outgoing-handler`.
    HttpOutgoing,
    /// `wasi:http/incoming-handler`.
    HttpIncoming,
    /// `wasi:sockets`.
    Sockets,
    /// `paddock:sql`.
    Sql,
    /// `paddock:metrics`.
    Metrics,
    /// `paddock:admin`.
    Admin,
    /// `paddock:groups`: local process groups and publish.
    Groups,
}

impl Capability {
    /// Every capability.
    pub const ALL: &'static [Self] = &[
        Self::Supervisor,
        Self::Logging,
        Self::Config,
        Self::Secrets,
        Self::KeyValue,
        Self::Filesystem,
        Self::HttpOutgoing,
        Self::HttpIncoming,
        Self::Sockets,
        Self::Sql,
        Self::Metrics,
        Self::Admin,
        Self::Groups,
    ];

    /// The key used under `capabilities` in the manifest and in operator
    /// policy.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Supervisor => "paddock:runtime/supervisor",
            Self::Logging => "wasi:logging",
            Self::Config => "wasi:config",
            Self::Secrets => "paddock:secrets",
            Self::KeyValue => "wasi:keyvalue",
            Self::Filesystem => "wasi:filesystem",
            Self::HttpOutgoing => "wasi:http/outgoing-handler",
            Self::HttpIncoming => "wasi:http/incoming-handler",
            Self::Sockets => "wasi:sockets",
            Self::Sql => "paddock:sql",
            Self::Metrics => "paddock:metrics",
            Self::Admin => "paddock:admin",
            Self::Groups => "paddock:groups",
        }
    }

    /// The capability with this key.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.key() == key)
    }

    /// The phase in which the host starts enforcing this capability, or
    /// `None` if Phase 1 enforces it.
    #[must_use]
    pub const fn phase(self) -> Option<Phase> {
        match self {
            Self::Logging
            | Self::Config
            | Self::KeyValue
            | Self::Filesystem
            | Self::Groups
            | Self::HttpIncoming
            | Self::Secrets
            | Self::Metrics => None,
            Self::HttpOutgoing => Some(Phase::Phase2),
            Self::Sockets | Self::Sql => Some(Phase::Phase3),
            Self::Supervisor | Self::Admin => Some(Phase::Later),
        }
    }

    /// The WIT package this capability covers and, if it covers only some of
    /// the package, which interfaces.
    const fn interfaces(self) -> (&'static str, Option<&'static [&'static str]>) {
        match self {
            Self::Supervisor => ("paddock:runtime", Some(&["supervisor"])),
            Self::Logging => ("wasi:logging", None),
            Self::Config => ("wasi:config", None),
            Self::Secrets => ("paddock:secrets", None),
            Self::KeyValue => ("wasi:keyvalue", None),
            Self::Filesystem => ("wasi:filesystem", None),
            // `wasi:http/types` comes with either handler.
            Self::HttpOutgoing => ("wasi:http", Some(&["outgoing-handler", "types"])),
            Self::HttpIncoming => ("wasi:http", Some(&["incoming-handler", "types"])),
            Self::Sockets => ("wasi:sockets", None),
            Self::Sql => ("paddock:sql", None),
            Self::Metrics => ("paddock:metrics", None),
            Self::Admin => ("paddock:admin", None),
            Self::Groups => ("paddock:groups", None),
        }
    }

    fn covers(self, name: &InterfaceName<'_>) -> bool {
        let (package, interfaces) = self.interfaces();
        name.package == package
            && interfaces.is_none_or(|list| name.interface.is_some_and(|i| list.contains(&i)))
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

impl From<Capability> for &'static str {
    fn from(c: Capability) -> Self {
        c.key()
    }
}

impl TryFrom<String> for Capability {
    type Error = String;

    fn try_from(key: String) -> Result<Self, Self::Error> {
        Self::from_key(&key).ok_or_else(|| format!("unknown capability '{key}'"))
    }
}

/// The index of [`Capability::KeyValue`] in [`Capability::ALL`].
const KEYVALUE: usize = 4;

/// Imports granted to every app without being declared: `(package,
/// interfaces)`, where `None` means the whole package.
const IMPLICIT: &[(&str, Option<&[&str]>)] = &[
    ("paddock:runtime", Some(&["types", "process"])),
    ("wasi:clocks", None),
    ("wasi:random", None),
    ("wasi:io", None),
    ("wasi:cli", None),
];

/// A component import name split into parts:
/// `wasi:keyvalue/store@0.2.0-draft` → package `wasi:keyvalue`, interface
/// `store`, version `0.2.0-draft`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceName<'a> {
    /// `namespace:package`.
    pub package: &'a str,
    /// Interface within the package, if named.
    pub interface: Option<&'a str>,
    /// Package version, if given.
    pub version: Option<&'a str>,
}

impl<'a> InterfaceName<'a> {
    /// Split an import name. `None` if it is not an interface name (a plain
    /// function or instance import such as `log`).
    #[must_use]
    pub fn parse(name: &'a str) -> Option<Self> {
        let (path, version) = match name.split_once('@') {
            Some((path, version)) => (path, Some(version)),
            None => (name, None),
        };
        let (package, interface) = match path.split_once('/') {
            Some((package, interface)) => (package, Some(interface)),
            None => (path, None),
        };
        let (namespace, pkg) = package.split_once(':')?;
        (!namespace.is_empty() && !pkg.is_empty()).then_some(Self {
            package,
            interface,
            version,
        })
    }
}

/// Namespaces whose interfaces only built-in capabilities provide.
pub const RESERVED_NAMESPACES: &[&str] = &["wasi", "paddock"];

/// Whether `key` names an extension package: `namespace:package`, outside
/// [`RESERVED_NAMESPACES`], with no interface or version.
#[must_use]
pub fn is_extension_key(key: &str) -> bool {
    InterfaceName::parse(key).is_some_and(|n| {
        n.interface.is_none()
            && n.version.is_none()
            && n.package
                .split_once(':')
                .is_some_and(|(ns, _)| !RESERVED_NAMESPACES.contains(&ns))
    })
}

/// How an import relates to the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImportClass {
    /// Granted to every app; no declaration needed.
    Implicit,
    /// Allowed if any of these capabilities is declared (more than one only
    /// for interfaces shared between capabilities, such as `wasi:http/types`).
    Capability(&'static [Capability]),
    /// An extension package (`example:greeting`), allowed if declared under
    /// `capabilities.extensions`.
    Extension(String),
    /// No host provides it.
    Unknown,
}

/// Classify one component import by its full name
/// (`wasi:keyvalue/store@0.2.0-draft`).
#[must_use]
pub fn classify_import(name: &str) -> ImportClass {
    let Some(parsed) = InterfaceName::parse(name) else {
        return ImportClass::Unknown;
    };
    let implicit = IMPLICIT.iter().any(|(package, interfaces)| {
        parsed.package == *package
            && interfaces.is_none_or(|list| parsed.interface.is_some_and(|i| list.contains(&i)))
    });
    if implicit {
        return ImportClass::Implicit;
    }
    if is_extension_key(parsed.package) {
        return ImportClass::Extension(parsed.package.to_string());
    }
    // Two capabilities share `wasi:http/types`; everything else maps to one.
    const HTTP_TYPES: &[Capability] = &[Capability::HttpOutgoing, Capability::HttpIncoming];
    if parsed.package == "wasi:http" && parsed.interface == Some("types") {
        return ImportClass::Capability(HTTP_TYPES);
    }
    // Watches and compare-and-swap reach the same buckets as keyvalue.
    if parsed.package == "paddock:watch" {
        return ImportClass::Capability(std::slice::from_ref(&Capability::ALL[KEYVALUE]));
    }
    Capability::ALL
        .iter()
        .position(|c| c.covers(&parsed))
        .map_or(ImportClass::Unknown, |i| {
            ImportClass::Capability(std::slice::from_ref(&Capability::ALL[i]))
        })
}

/// The `capabilities` section: each host interface the app requests, with
/// the scope that narrows it. Keys are [`Capability::key`] values.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// `paddock:runtime/supervisor`.
    #[serde(
        rename = "paddock:runtime/supervisor",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub supervisor: Option<SupervisorScope>,
    /// `wasi:logging`.
    #[serde(
        rename = "wasi:logging",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub logging: Option<LoggingScope>,
    /// `wasi:config`.
    #[serde(
        rename = "wasi:config",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub config: Option<ConfigScope>,
    /// `paddock:secrets`.
    #[serde(
        rename = "paddock:secrets",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub secrets: Option<SecretsScope>,
    /// `wasi:keyvalue`.
    #[serde(
        rename = "wasi:keyvalue",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keyvalue: Option<KeyValueScope>,
    /// `wasi:filesystem`.
    #[serde(
        rename = "wasi:filesystem",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub filesystem: Option<FilesystemScope>,
    /// `wasi:http/outgoing-handler`.
    #[serde(
        rename = "wasi:http/outgoing-handler",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub http_outgoing: Option<HttpOutgoingScope>,
    /// `wasi:http/incoming-handler`.
    #[serde(
        rename = "wasi:http/incoming-handler",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub http_incoming: Option<HttpIncomingScope>,
    /// `wasi:sockets`.
    #[serde(
        rename = "wasi:sockets",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sockets: Option<SocketsScope>,
    /// `paddock:sql`.
    #[serde(
        rename = "paddock:sql",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sql: Option<SqlScope>,
    /// `paddock:metrics`.
    #[serde(
        rename = "paddock:metrics",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub metrics: Option<MetricsScope>,
    /// `paddock:admin`.
    #[serde(
        rename = "paddock:admin",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub admin: Option<AdminScope>,
    /// `paddock:groups`.
    #[serde(
        rename = "paddock:groups",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub groups: Option<GroupsScope>,
    /// Extension packages the app uses, by package name
    /// (`"example:greeting"`), each with the scope its host service reads.
    /// Served only by nodes built with that service (`paddock-host`'s
    /// `Extension`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl Capabilities {
    /// The capabilities this section declares.
    #[must_use]
    pub fn declared(&self) -> Vec<Capability> {
        Capability::ALL
            .iter()
            .copied()
            .filter(|c| self.is_declared(*c))
            .collect()
    }

    /// Whether `capability` serves the app without any import: HTTP
    /// ingress delivers requests as calls (`wasi:http/incoming-handler`'s
    /// routes), so its grant cannot be read from the component's imports.
    #[must_use]
    pub const fn import_free(capability: Capability) -> bool {
        matches!(capability, Capability::HttpIncoming)
    }

    /// Whether `capability` is declared.
    #[must_use]
    pub fn is_declared(&self, capability: Capability) -> bool {
        match capability {
            Capability::Supervisor => self.supervisor.is_some(),
            Capability::Logging => self.logging.is_some(),
            Capability::Config => self.config.is_some(),
            Capability::Secrets => self.secrets.is_some(),
            Capability::KeyValue => self.keyvalue.is_some(),
            Capability::Filesystem => self.filesystem.is_some(),
            Capability::HttpOutgoing => self.http_outgoing.is_some(),
            Capability::HttpIncoming => self.http_incoming.is_some(),
            Capability::Sockets => self.sockets.is_some(),
            Capability::Sql => self.sql.is_some(),
            Capability::Metrics => self.metrics.is_some(),
            Capability::Admin => self.admin.is_some(),
            Capability::Groups => self.groups.is_some(),
        }
    }

    pub(crate) fn validate(&self, v: &mut Validator) {
        if let Some(s) = &self.supervisor {
            v.at(Capability::Supervisor.key(), |v| s.validate(v));
        }
        if let Some(s) = &self.logging {
            v.at(Capability::Logging.key(), |v| s.validate(v));
        }
        if let Some(s) = &self.config {
            v.at(Capability::Config.key(), |v| s.validate(v));
        }
        if let Some(s) = &self.secrets {
            v.at(Capability::Secrets.key(), |v| {
                v.unique_names("secrets", s.secrets.iter().map(String::as_str));
            });
        }
        if let Some(s) = &self.keyvalue {
            v.at(Capability::KeyValue.key(), |v| s.validate(v));
        }
        if let Some(s) = &self.filesystem {
            v.at(Capability::Filesystem.key(), |v| s.validate(v));
        }
        if let Some(s) = &self.http_outgoing {
            v.at(Capability::HttpOutgoing.key(), |v| s.validate(v));
        }
        if let Some(s) = &self.http_incoming {
            v.at(Capability::HttpIncoming.key(), |v| s.validate(v));
        }
        if let Some(s) = &self.sockets {
            v.at(Capability::Sockets.key(), |v| s.validate(v));
        }
        if let Some(s) = &self.sql {
            v.at(Capability::Sql.key(), |v| {
                v.name("datasource", &s.datasource);
                if s.max_connections == 0 {
                    v.error_at("max_connections", "must be greater than zero");
                }
            });
        }
        if let Some(s) = &self.metrics {
            let valid = s
                .prefix
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && s.prefix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid {
                v.error_at(
                    format!("{}.prefix", Capability::Metrics.key()),
                    "must be a metric name prefix (letters, digits, '_'; not starting with a digit)",
                );
            }
            if s.max_series == Some(0) {
                v.error_at(
                    format!("{}.max_series", Capability::Metrics.key()),
                    "must be greater than zero",
                );
            }
        }
        if let Some(s) = &self.groups {
            v.at(Capability::Groups.key(), |v| {
                v.unique_names("join", s.join.iter().map(String::as_str));
                v.unique_names("publish", s.publish.iter().map(String::as_str));
            });
        }
        for (key, scope) in &self.extensions {
            if !is_extension_key(key) {
                v.error_at(
                    format!("extensions.{key}"),
                    "must be a package name (`namespace:package`) outside the wasi and paddock namespaces",
                );
            }
            if !scope.is_object() {
                v.error_at(format!("extensions.{key}"), "must be a table");
            }
        }
    }
}

/// Scope of `paddock:runtime/supervisor`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SupervisorScope {
    /// Most children the app may run at once. Unset means no app-level cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_children: Option<u32>,
    /// Apps it may start as children.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_apps: Vec<String>,
}

impl SupervisorScope {
    fn validate(&self, v: &mut Validator) {
        if self.max_children == Some(0) {
            v.error_at("max_children", "must be greater than zero");
        }
        v.unique_names("allowed_apps", self.allowed_apps.iter().map(String::as_str));
    }
}

/// Scope of `wasi:logging`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingScope {
    /// Records above this level are dropped.
    pub max_level: LogLevel,
    /// Most records per second; the excess is dropped and counted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_per_sec: Option<u32>,
}

impl LoggingScope {
    fn validate(&self, v: &mut Validator) {
        if self.rate_limit_per_sec == Some(0) {
            v.error_at("rate_limit_per_sec", "must be greater than zero");
        }
    }
}

/// Scope of `wasi:config`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigScope {
    /// Node-level keys the app may read, by prefix. The app's own keys
    /// (`config.defaults`) are always readable.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub key_prefixes: Vec<String>,
}

impl ConfigScope {
    fn validate(&self, v: &mut Validator) {
        for (i, prefix) in self.key_prefixes.iter().enumerate() {
            if prefix.is_empty() {
                v.error_at(format!("key_prefixes[{i}]"), "must not be empty");
            }
        }
    }
}

/// Scope of `paddock:secrets`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsScope {
    /// Names of the secrets the app may resolve.
    pub secrets: Vec<String>,
}

/// Scope of `wasi:keyvalue`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeyValueScope {
    /// Buckets the app may open, by name. The host namespaces them per app.
    pub buckets: BTreeMap<String, Bucket>,
    /// Shared buckets the app uses, by name, read-only or read-write. The
    /// node's configuration defines each one (`[buckets.<name>]`) and
    /// operator policy grants each one; apps that use the same name see the
    /// same data.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub shared: BTreeMap<String, AccessMode>,
}

impl KeyValueScope {
    fn validate(&self, v: &mut Validator) {
        for (name, bucket) in &self.buckets {
            v.at("buckets", |v| {
                v.name(name.clone(), name);
                if bucket.quota.is_some_and(|q| q.bytes() == 0) {
                    v.error_at(format!("{name}.quota"), "must be greater than zero");
                }
            });
        }
        for name in self.shared.keys() {
            v.at("shared", |v| {
                v.name(name.clone(), name);
                if self.buckets.contains_key(name) {
                    v.error_at(
                        name.clone(),
                        "is also an app bucket; a bucket name opens one or the other",
                    );
                }
            });
        }
    }
}

/// One term-storage bucket.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bucket {
    /// Where the data lives.
    pub durability: Durability,
    /// Most bytes the bucket may hold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<ByteSize>,
}

/// Where a bucket's data lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// In host memory; survives instance crashes, not node restarts.
    #[default]
    Memory,
    /// On the node's disk; survives node restarts.
    Disk,
    /// Replicated across nodes. Not supported until Phase 3.
    Replicated,
}

/// Scope of `wasi:filesystem`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesystemScope {
    /// Directories visible to the guest, keyed by guest path (`/data`).
    pub preopens: BTreeMap<String, Preopen>,
}

impl FilesystemScope {
    fn validate(&self, v: &mut Validator) {
        for (path, preopen) in &self.preopens {
            v.at("preopens", |v| {
                v.at(path.clone(), |v| {
                    if !is_valid_guest_path(path) {
                        v.error(
                            "guest path must be absolute and normalized \
                             (no '.', '..' or empty segments, no trailing '/')",
                        );
                    }
                    v.name("volume", &preopen.volume);
                    if preopen.quota.is_some_and(|q| q.bytes() == 0) {
                        v.error_at("quota", "must be greater than zero");
                    }
                });
            });
        }
    }
}

/// Whether `path` is an absolute, normalized guest path other than `/`.
fn is_valid_guest_path(path: &str) -> bool {
    path.strip_prefix('/').is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .split('/')
                .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
    })
}

/// One preopened directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preopen {
    /// Host volume backing the directory, from node configuration.
    pub volume: String,
    /// Read-only or read-write.
    #[serde(default)]
    pub mode: AccessMode,
    /// Most bytes the volume may hold. Not enforced in Phase 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<ByteSize>,
}

/// File access mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    /// Read-only.
    #[default]
    Ro,
    /// Read-write.
    Rw,
}

/// Scope of `wasi:http/outgoing-handler`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpOutgoingScope {
    /// Origins or host patterns the app may reach (`https://payments.internal`,
    /// `*.internal`).
    pub allow: Vec<String>,
    /// HTTP methods allowed. Empty means any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// Most requests in flight at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
}

impl HttpOutgoingScope {
    fn validate(&self, v: &mut Validator) {
        for (i, entry) in self.allow.iter().enumerate() {
            if entry.trim().is_empty() {
                v.error_at(format!("allow[{i}]"), "must not be empty");
            }
        }
        for (i, method) in self.methods.iter().enumerate() {
            if method.is_empty() || !method.chars().all(|c| c.is_ascii_uppercase()) {
                v.error_at(
                    format!("methods[{i}]"),
                    format!("'{method}' is not an uppercase HTTP method"),
                );
            }
        }
        if self.max_concurrent == Some(0) {
            v.error_at("max_concurrent", "must be greater than zero");
        }
    }
}

/// Scope of `wasi:http/incoming-handler`: the routes the node's HTTP
/// ingress sends to the app, each request as a call.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpIncomingScope {
    /// Requests routed to the app.
    pub routes: Vec<Route>,
    /// Largest request body accepted (default 1 MiB, and never more than
    /// `limits.message_max_bytes` allows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_body: Option<ByteSize>,
    /// Time a request may wait for the app's reply before the client gets
    /// 504 (default 30000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl HttpIncomingScope {
    fn validate(&self, v: &mut Validator) {
        for (i, route) in self.routes.iter().enumerate() {
            if !route.path_prefix.starts_with('/') {
                v.error_at(format!("routes[{i}].path_prefix"), "must start with '/'");
            }
            if let Some(h) = &route.hash_header
                && (h.is_empty()
                    || !h
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)))
            {
                v.error_at(
                    format!("routes[{i}].hash_header"),
                    "must be a header name (letters, digits, '-' and '_')",
                );
            }
        }
        if self.routes.is_empty() {
            v.error_at("routes", "must list at least one route");
        }
        if self.max_body.is_some_and(|b| b.bytes() == 0) {
            v.error_at("max_body", "must be greater than zero");
        }
        if self.timeout_ms == Some(0) {
            v.error_at("timeout_ms", "must be greater than zero");
        }
    }
}

/// One HTTP route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Host to match. Unset matches any host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Path prefix to match (`/orders`), on segment boundaries: `/orders`
    /// matches `/orders` and `/orders/7`, not `/ordersx`.
    pub path_prefix: String,
    /// A request header whose value becomes the request's `key` field, for
    /// a pool routed with `hash:key` (a tenant or customer id). Unset sends
    /// requests round-robin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_header: Option<String>,
}

/// Scope of `wasi:sockets`. Bind and connect have separate rules: Rust's
/// standard library binds `0.0.0.0:0` before every connect, so one
/// destination-only rule would refuse every connection.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SocketsScope {
    /// Addresses the app may bind.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bind: Vec<SocketRule>,
    /// Addresses the app may connect to.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub connect: Vec<SocketRule>,
    /// Whether the app may listen on bound TCP sockets.
    pub listen: bool,
}

impl SocketsScope {
    fn validate(&self, v: &mut Validator) {
        for (list, rules) in [("bind", &self.bind), ("connect", &self.connect)] {
            for (i, rule) in rules.iter().enumerate() {
                if parse_cidr(&rule.cidr).is_none() {
                    v.error_at(
                        format!("{list}[{i}].cidr"),
                        format!("'{}' is not a CIDR block such as 10.0.0.0/8", rule.cidr),
                    );
                }
            }
        }
    }
}

/// An address block and port range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SocketRule {
    /// Address block (`10.0.0.0/8`, `::1/128`).
    pub cidr: String,
    /// Ports. Unset means any port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<PortRange>,
}

/// Parse `addr/prefix`, checking the prefix fits the address family.
#[must_use]
pub fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (addr, prefix) = s.split_once('/')?;
    let addr: IpAddr = addr.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    (prefix <= max).then_some((addr, prefix))
}

/// An inclusive port range, written `"443"` or `"8000-8100"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PortRange {
    /// First port.
    pub start: u16,
    /// Last port, inclusive.
    pub end: u16,
}

impl FromStr for PortRange {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || format!("invalid port range '{s}' (expected \"443\" or \"8000-8100\")");
        let (start, end) = s.split_once('-').unwrap_or((s, s));
        let start: u16 = start.trim().parse().map_err(|_| err())?;
        let end: u16 = end.trim().parse().map_err(|_| err())?;
        if end < start {
            return Err(err());
        }
        Ok(Self { start, end })
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

impl TryFrom<String> for PortRange {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<PortRange> for String {
    fn from(r: PortRange) -> Self {
        r.to_string()
    }
}

/// Scope of `paddock:sql`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlScope {
    /// Named datasource from node configuration.
    pub datasource: String,
    /// Most connections the app may hold.
    pub max_connections: u32,
}

/// Scope of `paddock:metrics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsScope {
    /// Every metric the app emits is named under this prefix
    /// (`<prefix>_<name>`).
    pub prefix: String,
    /// Most series (a name with one set of label values) the app may hold;
    /// unset, 1,000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_series: Option<u32>,
}

/// Scope of `paddock:groups`: the groups the app may use, by name. Groups
/// are created by joining; nothing in node configuration binds them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GroupsScope {
    /// Groups the app's instances may join and leave.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub join: Vec<String>,
    /// Groups the app may publish to. Listing members is allowed for a
    /// group in either list.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub publish: Vec<String>,
}

impl GroupsScope {
    /// Whether the app may join `group`.
    #[must_use]
    pub fn may_join(&self, group: &str) -> bool {
        self.join.iter().any(|g| g == group)
    }

    /// Whether the app may publish to `group`.
    #[must_use]
    pub fn may_publish(&self, group: &str) -> bool {
        self.publish.iter().any(|g| g == group)
    }
}

/// Scope of `paddock:admin`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminScope {
    /// What the app may do through the admin interface.
    pub access: AdminAccess,
}

/// Admin access level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdminAccess {
    /// Inspect only.
    #[default]
    ReadOnly,
    /// Deploy, stop, upgrade.
    Control,
}

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod capabilities_tests;
