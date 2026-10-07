//! What one instance runs with.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use paddock_bundle::manifest::{AccessMode, AppManifest, AppSpec, Capability};

use crate::services::ServiceSpecs;

/// Limits, timeouts and `init` arguments for one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    /// App name, for log records.
    pub app: String,
    /// App version, for log records.
    pub version: String,
    /// Most linear memory, in bytes.
    pub memory_max: u64,
    /// Most table elements. `None` leaves the engine default.
    pub table_max: Option<u32>,
    /// Guest execution time allowed per `handle-*` callback.
    pub callback_deadline: Duration,
    /// Mailbox capacity.
    pub mailbox_max: usize,
    /// Largest message body the instance accepts, in bytes.
    pub message_max_bytes: u64,
    /// Names the instance may register.
    pub registered_names: Vec<String>,
    /// Time allowed for `init`.
    pub init_timeout: Duration,
    /// Time allowed for `terminate` before the instance is killed.
    pub shutdown_timeout: Duration,
    /// Passed to `init`.
    pub init_args: Vec<u8>,
    /// Capabilities the host serves. A declared capability missing here is
    /// denied: `wasi:logging` drops records, `wasi:config` and
    /// `wasi:keyvalue` return `access-denied`, `wasi:filesystem` gets no
    /// preopens.
    pub granted: BTreeSet<Capability>,
    /// Directories visible to the guest, used when `wasi:filesystem` is
    /// granted.
    pub preopens: Vec<Preopen>,
    /// What the manifest asks of each host service.
    pub services: ServiceSpecs,
}

/// A host directory visible to the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preopen {
    /// The directory on the host.
    pub host: PathBuf,
    /// Where the guest sees it (`/data`).
    pub guest: String,
    /// Read-write if `true`, read-only otherwise.
    pub writable: bool,
}

/// The guest path of the bundle's read-only files.
pub const PRIV_PATH: &str = "/priv";

impl InstanceSpec {
    /// A spec with the manifest defaults (64 MiB, 5 s deadlines, a
    /// 10,000-message mailbox) and no capabilities granted.
    #[must_use]
    pub fn new(app: impl Into<String>, version: impl Into<String>) -> Self {
        let mut spec = Self::from_parts(app.into(), version.into(), &AppSpec::default());
        spec.granted.clear();
        spec
    }

    /// The spec a verified manifest describes, with every declared
    /// capability granted (narrow it with [`InstanceSpec::with_granted`]).
    /// `lifecycle.start_args` is passed to `init` as JSON; absent args are an
    /// empty list. Every name in `registered_names` may be registered on this
    /// node; global scope is not enforced until Phase 2. `wasi:config`
    /// returns the manifest's `config.defaults` (merge node overrides into
    /// `services.config.values`). Preopens need
    /// [`InstanceSpec::resolve_preopens`].
    #[must_use]
    pub fn from_manifest(manifest: &AppManifest) -> Self {
        Self::from_parts(
            manifest.app.name.clone(),
            manifest.app.version.to_string(),
            &manifest.spec,
        )
    }

    fn from_parts(app: String, version: String, spec: &AppSpec) -> Self {
        Self {
            app,
            version,
            memory_max: spec.limits.memory_max.bytes(),
            table_max: spec.limits.table_max,
            callback_deadline: Duration::from_millis(spec.limits.callback_deadline_ms),
            mailbox_max: usize::try_from(spec.limits.mailbox_max).unwrap_or(usize::MAX),
            message_max_bytes: spec.limits.message_max_bytes.bytes(),
            registered_names: spec
                .registered_names
                .iter()
                .map(|n| n.name.clone())
                .collect(),
            init_timeout: Duration::from_millis(spec.lifecycle.init_timeout_ms),
            shutdown_timeout: Duration::from_millis(spec.lifecycle.shutdown_timeout_ms),
            init_args: spec
                .lifecycle
                .start_args
                .as_ref()
                .map(|v| v.to_string().into_bytes())
                .unwrap_or_default(),
            granted: spec.capabilities.declared().into_iter().collect(),
            preopens: Vec::new(),
            services: ServiceSpecs::from_manifest(spec),
        }
    }

    /// Keep only the capabilities in `granted` (the operator's effective
    /// grant).
    #[must_use]
    pub fn with_granted(mut self, granted: &BTreeSet<Capability>) -> Self {
        self.granted.retain(|c| granted.contains(c));
        self
    }

    /// Set the preopens a manifest's `wasi:filesystem` scope asks for:
    /// `priv_dir` (the bundle's files) read-only at `/priv`, and each
    /// declared preopen backed by the host directory `volume` resolves its
    /// volume name to.
    ///
    /// # Errors
    ///
    /// A message for a volume `volume` cannot resolve, or a preopen at the
    /// reserved `/priv`.
    pub fn resolve_preopens(
        &mut self,
        manifest: &AppManifest,
        volume: impl Fn(&str) -> Option<PathBuf>,
        priv_dir: Option<&Path>,
    ) -> Result<(), String> {
        let Some(fs) = &manifest.spec.capabilities.filesystem else {
            self.preopens.clear();
            return Ok(());
        };
        let mut preopens = Vec::new();
        if let Some(dir) = priv_dir {
            preopens.push(Preopen {
                host: dir.to_path_buf(),
                guest: PRIV_PATH.to_string(),
                writable: false,
            });
        }
        for (guest, preopen) in &fs.preopens {
            if guest == PRIV_PATH {
                return Err(format!("{PRIV_PATH} is reserved for the bundle's files"));
            }
            let host = volume(&preopen.volume).ok_or_else(|| {
                format!("volume '{}' is not configured on this node", preopen.volume)
            })?;
            preopens.push(Preopen {
                host,
                guest: guest.clone(),
                writable: preopen.mode == AccessMode::Rw,
            });
        }
        self.preopens = preopens;
        Ok(())
    }
}
