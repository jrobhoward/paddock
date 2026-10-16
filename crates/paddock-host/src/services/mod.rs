//! Host services: the guest-facing interfaces apps import beside `process`.
//!
//! Each service is a module with a fixed shape, described by the crate-private `Service` trait:
//!
//! - the capability that grants it, and the WIT interfaces it links;
//! - a `Spec`: what the manifest asks of the service for one app, carried in
//!   [`InstanceSpec::services`](crate::InstanceSpec::services);
//! - `Shared` state, one per [`Runtime`](crate::Runtime): pools, limits,
//!   operator overrides;
//! - `Instance` state, one per instance, built from the spec and the shared
//!   state;
//! - `link`: its own `bindgen!` world, linked with the per-interface
//!   `add_to_linker` and a getter that hands each host function a
//!   `ServiceCtx`.
//!
//! Host functions see only their own state and the instance facilities every
//! service shares (tags, the callback clock, the resource table), never the
//! rest of the instance. Linking happens once per engine, so a host call is a
//! direct call on the service's state.
//!
//! Every service follows the same rules:
//!
//! - A capability the operator did not grant is still linked and fails
//!   softly: an `access-denied` error value where the interface has one,
//!   never a trap.
//! - Time spent waiting in host I/O is host time
//!   (`ServiceCtx::add_host_time`), not counted against the callback
//!   deadline.
//! - Named resources (buckets, datasources) are declared in the manifest and
//!   bound in node configuration, so an app does not name host paths or
//!   addresses.
//!
//! Services from outside this crate are [`Extension`]s ([`extension`]):
//! the same shape behind an object-safe trait, registered at runtime.
//!
//! Adding a service is a new module here and one line in each of the
//! registries below ([`ServiceSpecs`], `Shared`, `States` and its
//! `announce`, `link`, `INTERFACES`); the process core and orchestration do not change.

pub mod config;
pub mod extension;
pub mod groups;
pub mod keyvalue;
pub mod logging;
pub mod metrics;
pub mod secrets;
pub mod sql;

pub use extension::{
    Extension, ExtensionApp, ExtensionCtx, ExtensionNode, ExtensionSpec, StartedExtensions,
    StoreData, extension_ctx,
};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use paddock_bundle::manifest::{AppSpec, Capability};
use wasmtime::component::{HasData, Linker, ResourceTable};

use crate::config::{HostObjects, RuntimeConfig};
use crate::error::HostError;
use crate::instance::InstanceHandle;
use crate::message::Envelope;
use crate::output::Tags;
use crate::pid::Pid;
use crate::spec::InstanceSpec;
use crate::state::CallbackClock;

/// One host service. See the module documentation.
pub(crate) trait Service: Sized + 'static {
    /// The capability that grants the service.
    const CAPABILITY: Capability;
    /// The interfaces the service links, as `package/interface` without a
    /// version. Imports of these are never deny-stubbed.
    const INTERFACES: &'static [&'static str];
    /// What the manifest asks of the service for one app.
    type Spec;
    /// Node-wide state, one per runtime.
    type Shared: Send + Sync;
    /// Per-instance state.
    type Instance: Send + 'static;

    /// The spec a manifest describes.
    fn spec(manifest: &AppSpec) -> Self::Spec;

    /// Node-wide state for a new runtime.
    ///
    /// # Errors
    ///
    /// A [`HostError`] if the service cannot start, for example storage
    /// that cannot open.
    fn shared(config: &RuntimeConfig) -> Result<Self::Shared, HostError>;

    /// State for a new instance (or for the new Store of an upgrade).
    fn instance(
        shared: &Arc<Self::Shared>,
        spec: &Self::Spec,
        info: &InstanceInfo<'_>,
    ) -> Self::Instance;

    /// Link the service's interfaces, reaching its state through `T`.
    ///
    /// # Errors
    ///
    /// A wasmtime error if an interface is already linked.
    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> wasmtime::Result<()>;

    /// Host events to send the instance as `info.host-event`: after `init`,
    /// after a snapshot upgrade (the new guest starts fresh), and when the
    /// runtime asks the instance to announce again (an operator change).
    /// None by default.
    fn announce(_state: &Self::Instance, _tags: &Tags) -> Vec<(String, Envelope)> {
        Vec::new()
    }

    /// The instance `pid` exited: drop node-wide state it held. Not called
    /// for an upgrade, which keeps the pid. Nothing by default.
    fn exited(_shared: &Self::Shared, _pid: Pid) {}
}

/// The instance a service's per-instance state is for.
pub(crate) struct InstanceInfo<'a> {
    /// Whether the operator granted the service's capability.
    pub(crate) granted: bool,
    /// The instance's handle, for services that deliver messages to it.
    pub(crate) handle: &'a InstanceHandle,
    /// The instance's host-object limits.
    pub(crate) host_objects: HostObjects,
    /// The node's metrics.
    pub(crate) metrics: &'a crate::metrics::Metrics,
}

/// What a service's host functions see: the service's own per-instance
/// state and the facilities every service shares.
pub struct ServiceCtx<'a, I> {
    /// The service's own per-instance state.
    pub(crate) state: &'a mut I,
    /// Who the instance is, for log records and errors.
    pub(crate) tags: &'a Tags,
    /// Resources handed to the guest.
    pub(crate) table: &'a mut ResourceTable,
    clock: &'a mut CallbackClock,
}

impl<I> std::fmt::Debug for ServiceCtx<'_, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceCtx")
            .field("app", &self.tags.app)
            .field("pid", &self.tags.pid)
            .finish_non_exhaustive()
    }
}

impl<'a, I> ServiceCtx<'a, I> {
    /// The service's per-instance state.
    pub fn state(&mut self) -> &mut I {
        self.state
    }

    /// The instance's app name.
    #[must_use]
    pub fn app(&self) -> &str {
        &self.tags.app
    }

    /// The instance's app version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.tags.version
    }

    /// The instance's pid.
    #[must_use]
    pub fn pid(&self) -> Pid {
        self.tags.pid
    }

    /// The instance's resource table, for resources handed to the guest.
    pub fn table(&mut self) -> &mut ResourceTable {
        self.table
    }

    pub(crate) fn new(
        state: &'a mut I,
        tags: &'a Tags,
        table: &'a mut ResourceTable,
        clock: &'a mut CallbackClock,
    ) -> Self {
        Self {
            state,
            tags,
            table,
            clock,
        }
    }

    /// Exclude time spent waiting in the host from the callback deadline.
    pub fn add_host_time(&mut self, spent: Duration) {
        self.clock.add_host_time(spent);
    }
}

/// Store data that holds the state of service `S`.
pub(crate) trait HostState<S: Service>: Send + 'static {
    /// The context `S`'s host functions run with.
    fn service(&mut self) -> ServiceCtx<'_, S::Instance>;
}

/// The [`HasData`] projection from store data to [`ServiceCtx`].
pub(crate) struct Ctx<S>(std::marker::PhantomData<S>);

impl<S: Service> HasData for Ctx<S> {
    type Data<'a> = ServiceCtx<'a, S::Instance>;
}

/// What the manifest asks of each service, for one instance.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ServiceSpecs {
    /// `wasi:config`.
    pub config: config::ConfigSpec,
    /// `paddock:groups`.
    pub groups: groups::GroupsSpec,
    /// `wasi:keyvalue`.
    pub keyvalue: keyvalue::KeyValueSpec,
    /// `wasi:logging`.
    pub logging: logging::LoggingSpec,
    /// `paddock:secrets`.
    pub secrets: secrets::SecretsSpec,
    /// `paddock:metrics`.
    pub metrics: metrics::MetricsSpec,
    /// `paddock:sql`.
    pub sql: sql::SqlSpec,
    /// Extensions the manifest declares, by package.
    pub extensions: BTreeMap<String, ExtensionSpec>,
}

impl ServiceSpecs {
    pub(crate) fn from_manifest(manifest: &AppSpec) -> Self {
        Self {
            config: config::Config::spec(manifest),
            groups: groups::Groups::spec(manifest),
            keyvalue: keyvalue::KeyValue::spec(manifest),
            logging: logging::Logging::spec(manifest),
            secrets: secrets::Secrets::spec(manifest),
            metrics: metrics::MetricsService::spec(manifest),
            sql: sql::Sql::spec(manifest),
            extensions: manifest
                .capabilities
                .extensions
                .iter()
                .map(|(package, scope)| (package.clone(), ExtensionSpec::new(scope.clone())))
                .collect(),
        }
    }
}

/// Every service's node-wide state.
pub(crate) struct Shared {
    config: Arc<()>,
    groups: Arc<groups::Shared>,
    keyvalue: Arc<<keyvalue::KeyValue as Service>::Shared>,
    pub(crate) logging: Arc<logging::Shared>,
    secrets: Arc<<secrets::Secrets as Service>::Shared>,
    metrics: Arc<()>,
    sql: Arc<sql::Shared>,
}

impl Shared {
    pub(crate) fn new(config: &RuntimeConfig) -> Result<Self, HostError> {
        Ok(Self {
            config: Arc::new(config::Config::shared(config)?),
            groups: Arc::new(groups::Groups::shared(config)?),
            keyvalue: Arc::new(keyvalue::KeyValue::shared(config)?),
            logging: Arc::new(logging::Logging::shared(config)?),
            secrets: Arc::new(secrets::Secrets::shared(config)?),
            metrics: Arc::new(metrics::MetricsService::shared(config)?),
            sql: Arc::new(sql::Sql::shared(config)?),
        })
    }

    /// Process groups, which other nodes' frames update (ROADMAP D4).
    pub(crate) fn groups(&self) -> &Arc<groups::Shared> {
        &self.groups
    }

    /// Tell every service that `pid` exited.
    pub(crate) fn exited(&self, pid: Pid) {
        config::Config::exited(&self.config, pid);
        groups::Groups::exited(&self.groups, pid);
        keyvalue::KeyValue::exited(&self.keyvalue, pid);
        logging::Logging::exited(&self.logging, pid);
        secrets::Secrets::exited(&self.secrets, pid);
        metrics::MetricsService::exited(&self.metrics, pid);
        sql::Sql::exited(&self.sql, pid);
    }
}

/// Every service's state for one instance.
pub(crate) struct States {
    pub(crate) config: config::Instance,
    pub(crate) groups: groups::Instance,
    pub(crate) keyvalue: keyvalue::Instance,
    pub(crate) logging: logging::Instance,
    pub(crate) secrets: secrets::Instance,
    pub(crate) metrics: metrics::Instance,
    pub(crate) sql: sql::Instance,
    pub(crate) extensions: extension::ExtensionStates,
}

impl States {
    /// Every service's announcements, in registry order.
    pub(crate) fn announce(&self, tags: &Tags) -> Vec<(String, Envelope)> {
        let mut events = config::Config::announce(&self.config, tags);
        events.extend(groups::Groups::announce(&self.groups, tags));
        events.extend(keyvalue::KeyValue::announce(&self.keyvalue, tags));
        events.extend(logging::Logging::announce(&self.logging, tags));
        events.extend(secrets::Secrets::announce(&self.secrets, tags));
        events.extend(metrics::MetricsService::announce(&self.metrics, tags));
        events.extend(sql::Sql::announce(&self.sql, tags));
        events
    }

    /// # Errors
    ///
    /// The message of an extension that refused the instance's scope.
    pub(crate) fn new(
        shared: &Shared,
        spec: &InstanceSpec,
        extensions: &[Arc<dyn Extension>],
        handle: &InstanceHandle,
        host_objects: HostObjects,
        metrics: &crate::metrics::Metrics,
    ) -> Result<Self, String> {
        let info = |capability: Capability| InstanceInfo {
            granted: spec.granted.contains(&capability),
            handle,
            host_objects,
            metrics,
        };
        let null = serde_json::Value::Null;
        let mut states = extension::ExtensionStates::default();
        for ext in extensions {
            let declared = spec.services.extensions.get(ext.package());
            let app = ExtensionApp {
                app: &spec.app,
                version: &spec.version,
                scope: declared.map_or(&null, |d| &d.scope),
                granted: declared.is_some_and(|d| d.granted),
            };
            let state = ext
                .instance(&app)
                .map_err(|e| format!("extension {}: {e}", ext.package()))?;
            states.push(state);
        }
        let services = &spec.services;
        Ok(Self {
            config: config::Config::instance(
                &shared.config,
                &services.config,
                &info(config::Config::CAPABILITY),
            ),
            groups: groups::Groups::instance(
                &shared.groups,
                &services.groups,
                &info(groups::Groups::CAPABILITY),
            ),
            keyvalue: keyvalue::KeyValue::instance(
                &shared.keyvalue,
                &services.keyvalue,
                &info(keyvalue::KeyValue::CAPABILITY),
            ),
            logging: logging::Logging::instance(
                &shared.logging,
                &services.logging,
                &info(logging::Logging::CAPABILITY),
            ),
            secrets: secrets::Secrets::instance(
                &shared.secrets,
                &services.secrets,
                &info(secrets::Secrets::CAPABILITY),
            ),
            metrics: metrics::MetricsService::instance(
                &shared.metrics,
                &services.metrics,
                &info(metrics::MetricsService::CAPABILITY),
            ),
            sql: sql::Sql::instance(&shared.sql, &services.sql, &info(sql::Sql::CAPABILITY)),
            extensions: states,
        })
    }
}

/// Link every service.
pub(crate) fn link<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: HostState<config::Config>
        + HostState<groups::Groups>
        + HostState<keyvalue::KeyValue>
        + HostState<logging::Logging>
        + HostState<secrets::Secrets>
        + HostState<metrics::MetricsService>
        + HostState<sql::Sql>,
{
    config::Config::link(linker)?;
    groups::Groups::link(linker)?;
    keyvalue::KeyValue::link(linker)?;
    logging::Logging::link(linker)?;
    secrets::Secrets::link(linker)?;
    metrics::MetricsService::link(linker)?;
    sql::Sql::link(linker)?;
    Ok(())
}

/// The interfaces the services link, `package/interface`.
pub(crate) const INTERFACES: &[&[&str]] = &[
    config::Config::INTERFACES,
    groups::Groups::INTERFACES,
    keyvalue::KeyValue::INTERFACES,
    logging::Logging::INTERFACES,
    secrets::Secrets::INTERFACES,
    metrics::MetricsService::INTERFACES,
    sql::Sql::INTERFACES,
];
