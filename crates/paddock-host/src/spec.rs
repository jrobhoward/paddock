//! What one instance runs with.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use paddock_bundle::manifest::{AccessMode, AppManifest, AppSpec, Capability};

use crate::acl::Acl;
use crate::config::HostObjectRequest;
use crate::health::HealthCheck;
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
    /// Host-object limits the manifest asks for.
    pub host_objects: HostObjectRequest,
    /// Tell the instance its node's failover role, as
    /// `host-event("failover-role", "primary" | "standby" | "down")`, after
    /// `init` and on every change ([`crate::Runtime::set_failover_role`]):
    /// a `hot-standby` app.
    pub announce_role: bool,
    /// Run guest code only while this node's failover lease is current: a
    /// `singleton` app. The instance checks the lease against the
    /// monotonic clock before `init`, each callback and `terminate`, and
    /// at every epoch tick. Out of lease, it waits before a callback (the
    /// message stays queued) until the lease is renewed or the node's gate
    /// kills it, and a callback running when the lease ends is ended with
    /// `crashed: lease_ended` at the next tick. No effect on a node without
    /// failover.
    pub bound_to_lease: bool,
    /// Check the instance's health on a timer (the manifest's `health`
    /// section; [`crate::health`]).
    pub health: Option<HealthCheck>,
    /// Which apps this instance may send to and accept from (the
    /// manifest's `messaging_acl`; [`crate::acl`]).
    pub acl: Acl,
    /// Passivate the instance after this long with nothing to do (spike
    /// S11): run `save-snapshot`, drop the Store and keep the pid, mailbox,
    /// links, monitors, names and timers; the next message, signal or
    /// operation builds a new Store and runs `load-snapshot` with the
    /// instance's own version. Only for an app whose `load-snapshot` reads
    /// its own snapshots. An instance waiting to answer a deferred call or
    /// holding a host resource (a bucket, a watch) stays live. `None`
    /// never passivates.
    pub passivate_after: Option<Duration>,
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
            announce_role: spec
                .placement
                .as_ref()
                .is_some_and(|p| p.mode == paddock_bundle::manifest::PlacementMode::HotStandby),
            bound_to_lease: spec
                .placement
                .as_ref()
                .is_some_and(|p| p.mode == paddock_bundle::manifest::PlacementMode::Singleton),
            acl: spec
                .messaging_acl
                .as_ref()
                .map(|a| Acl {
                    accept_from: a.accept_from.clone(),
                    may_call: a.may_call.clone(),
                })
                .unwrap_or_default(),
            passivate_after: spec.instances.passivate_after_ms.map(Duration::from_millis),
            health: spec.health.as_ref().map(|h| HealthCheck {
                interval: Duration::from_millis(h.interval_ms),
                failure_threshold: h.failure_threshold,
            }),
            host_objects: HostObjectRequest {
                timers: spec.limits.timers_max,
                monitors: spec.limits.monitors_max,
                links: spec.limits.links_max,
                pending_calls: spec.limits.pending_calls_max,
                groups: spec.limits.groups_max,
                watches: spec.limits.watches_max,
            },
        }
    }

    /// Keep only the capabilities in `granted` (the operator's effective
    /// grant).
    #[must_use]
    pub fn with_granted(mut self, granted: &BTreeSet<Capability>) -> Self {
        self.granted.retain(|c| granted.contains(c));
        self
    }

    /// Keep only the extensions in `granted` granted (the operator's
    /// effective grant); the others stay declared but denied.
    #[must_use]
    pub fn with_granted_extensions(mut self, granted: &BTreeSet<String>) -> Self {
        for (package, ext) in &mut self.services.extensions {
            ext.granted = ext.granted && granted.contains(package);
        }
        self
    }

    /// Keep only the secrets in `granted` readable (the effective grant's
    /// `granted_secrets`); the others return `access-denied`.
    #[must_use]
    pub fn with_granted_secrets(mut self, granted: &BTreeSet<String>) -> Self {
        self.services
            .secrets
            .names
            .retain(|name| granted.contains(name));
        self
    }

    /// Keep only the SQL datasources in `granted` usable (the effective
    /// grant's `granted_datasources`); the others return `access-denied`.
    #[must_use]
    pub fn with_granted_datasources(mut self, granted: &BTreeSet<String>) -> Self {
        self.services
            .sql
            .datasources
            .retain(|name, _| granted.contains(name));
        self
    }

    /// Keep only the shared buckets in `granted` openable (the effective
    /// grant's `granted_buckets`); the others return `access-denied`.
    #[must_use]
    pub fn with_granted_buckets(mut self, granted: &BTreeSet<String>) -> Self {
        self.services
            .keyvalue
            .shared
            .retain(|name, _| granted.contains(name));
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
