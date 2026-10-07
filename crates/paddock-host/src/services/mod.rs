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
//! Adding a service is a new module here and one line in each of the
//! registries below ([`ServiceSpecs`], `Shared`, `States`, `link`,
//! `INTERFACES`); the process core and orchestration do not change.

pub mod logging;

use std::sync::Arc;
use std::time::Duration;

use paddock_bundle::manifest::{AppSpec, Capability};
use wasmtime::component::{HasData, Linker, ResourceTable};

use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::output::Tags;
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

    /// State for a new instance; `granted` says whether the operator granted
    /// [`Service::CAPABILITY`].
    fn instance(shared: &Arc<Self::Shared>, spec: &Self::Spec, granted: bool) -> Self::Instance;

    /// Link the service's interfaces, reaching its state through `T`.
    ///
    /// # Errors
    ///
    /// A wasmtime error if an interface is already linked.
    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> wasmtime::Result<()>;
}

/// What a service's host functions see.
pub(crate) struct ServiceCtx<'a, I> {
    /// The service's own per-instance state.
    pub(crate) state: &'a mut I,
    /// Who the instance is, for log records and errors.
    pub(crate) tags: &'a Tags,
    /// Resources handed to the guest.
    #[allow(dead_code)] // First used by keyvalue when it moves here (A2).
    pub(crate) table: &'a mut ResourceTable,
    clock: &'a mut CallbackClock,
}

impl<'a, I> ServiceCtx<'a, I> {
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
    #[allow(dead_code)] // First used by keyvalue when it moves here (A2).
    pub(crate) fn add_host_time(&mut self, spent: Duration) {
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
    /// `wasi:logging`.
    pub logging: logging::LoggingSpec,
}

impl ServiceSpecs {
    pub(crate) fn from_manifest(manifest: &AppSpec) -> Self {
        Self {
            logging: logging::Logging::spec(manifest),
        }
    }
}

/// Every service's node-wide state.
pub(crate) struct Shared {
    pub(crate) logging: Arc<logging::Shared>,
}

impl Shared {
    pub(crate) fn new(config: &RuntimeConfig) -> Result<Self, HostError> {
        Ok(Self {
            logging: Arc::new(logging::Logging::shared(config)?),
        })
    }
}

/// Every service's state for one instance.
pub(crate) struct States {
    pub(crate) logging: logging::Instance,
}

impl States {
    pub(crate) fn new(shared: &Shared, spec: &InstanceSpec) -> Self {
        Self {
            logging: instance::<logging::Logging>(&shared.logging, &spec.services.logging, spec),
        }
    }
}

fn instance<S: Service>(
    shared: &Arc<S::Shared>,
    spec: &S::Spec,
    instance: &InstanceSpec,
) -> S::Instance {
    S::instance(shared, spec, instance.granted.contains(&S::CAPABILITY))
}

/// Link every service.
pub(crate) fn link<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: HostState<logging::Logging>,
{
    logging::Logging::link(linker)?;
    Ok(())
}

/// The interfaces the services link, `package/interface`.
pub(crate) const INTERFACES: &[&[&str]] = &[logging::Logging::INTERFACES];
