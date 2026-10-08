//! Host services from outside `paddock-host`: extensions.
//!
//! An extension serves one WIT package outside the reserved `wasi` and
//! `paddock` namespaces (`example:greeting`). Apps declare it under
//! `capabilities.extensions."<package>"` with a scope table the extension
//! reads, and the operator grants it by package name in the policy's
//! `grant` lists, like a built-in capability. A node built without the
//! extension links deny stubs for the package instead.
//!
//! An extension crate depends on `paddock-host` alone:
//!
//! 1. Generate host bindings for its package with
//!    `paddock_host::wasmtime::component::bindgen!`.
//! 2. Implement the generated `Host` traits for `ServiceCtx<'_, MyState>`,
//!    where `MyState` is the per-instance state [`Extension::instance`]
//!    returns. Return the interface's access-denied error when
//!    [`ExtensionApp::granted`] was false.
//! 3. In [`Extension::link`], call the generated
//!    `add_to_linker::<StoreData, ExtensionCtx<MyState>>(linker, extension_ctx::<MyState>)`.
//! 4. Pass the extension to [`Runtime::with_extensions`](crate::Runtime::with_extensions)
//!    (or to the `paddock` crate's node builder).
//!
//! Extensions follow the rules of the built-in services
//! ([`crate::services`]): fail softly when not granted, count time spent
//! waiting as host time ([`ServiceCtx::add_host_time`]), and read named
//! resources from the scope rather than host paths.

use std::any::{Any, TypeId};
use std::marker::PhantomData;

use wasmtime::component::{HasData, Linker};

use super::ServiceCtx;
use crate::state::InstanceState;

/// The data every instance's Store holds. Opaque: an extension reaches its
/// own state through [`extension_ctx`].
pub type StoreData = InstanceState;

/// A host service for one WIT package, provided from outside
/// `paddock-host`. See the module documentation.
pub trait Extension: Send + Sync + 'static {
    /// The WIT package served, `namespace:package` (`example:greeting`): the
    /// key apps declare under `capabilities.extensions` and operators grant.
    fn package(&self) -> &str;

    /// Link the package's interfaces. Host functions reach their state with
    /// [`extension_ctx`].
    ///
    /// # Errors
    ///
    /// A wasmtime error if an interface cannot be linked.
    fn link(&self, linker: &mut Linker<StoreData>) -> wasmtime::Result<()>;

    /// State for a new instance. Every instance gets one, including apps
    /// that do not declare the extension (with a null scope and `granted`
    /// false), so host functions always find it.
    ///
    /// # Errors
    ///
    /// A message if the scope is invalid; the instance then fails to start.
    fn instance(&self, app: &ExtensionApp<'_>) -> Result<Box<dyn Any + Send>, String>;
}

/// What an extension knows about an instance when it builds its state.
#[derive(Debug)]
#[non_exhaustive]
pub struct ExtensionApp<'a> {
    /// App name.
    pub app: &'a str,
    /// App version.
    pub version: &'a str,
    /// The app's `capabilities.extensions."<package>"` table; null if the
    /// app does not declare the extension.
    pub scope: &'a serde_json::Value,
    /// Whether the operator granted the extension to this app.
    pub granted: bool,
}

/// What the manifest asks of one extension for one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExtensionSpec {
    /// The `capabilities.extensions."<package>"` table.
    pub scope: serde_json::Value,
    /// Whether the operator granted it. Declared extensions start granted;
    /// narrow with [`InstanceSpec::with_granted_extensions`](crate::InstanceSpec::with_granted_extensions).
    pub granted: bool,
}

impl ExtensionSpec {
    /// A declared extension with `scope`, granted.
    #[must_use]
    pub fn new(scope: serde_json::Value) -> Self {
        Self {
            scope,
            granted: true,
        }
    }
}

/// The [`HasData`] projection for an extension whose per-instance state is
/// `S`; pass it to the generated `add_to_linker`.
#[derive(Debug)]
pub struct ExtensionCtx<S>(PhantomData<fn() -> S>);

impl<S: Send + 'static> HasData for ExtensionCtx<S> {
    type Data<'a> = ServiceCtx<'a, S>;
}

/// The context an extension's host function runs with: its state of type
/// `S` and the instance's shared facilities. Pass as the getter to the
/// generated `add_to_linker`.
///
/// # Panics
///
/// If no extension created state of type `S` for this instance, which
/// means `link` and `instance` of one extension disagree about the type.
// The getter has no error channel, and a mismatch is a bug in the extension.
#[allow(clippy::expect_used)]
pub fn extension_ctx<S: Send + 'static>(data: &mut StoreData) -> ServiceCtx<'_, S> {
    data.extension_ctx::<S>()
        .expect("extension state type must match what `Extension::instance` returned")
}

/// Every extension's state for one instance, by type.
#[derive(Default)]
pub(crate) struct ExtensionStates(Vec<(TypeId, Box<dyn Any + Send>)>);

impl ExtensionStates {
    pub(crate) fn push(&mut self, state: Box<dyn Any + Send>) {
        let id = (*state).type_id();
        self.0.push((id, state));
    }

    pub(crate) fn get_mut<S: 'static>(&mut self) -> Option<&mut S> {
        let id = TypeId::of::<S>();
        self.0
            .iter_mut()
            .find(|(t, _)| *t == id)
            .and_then(|(_, s)| s.downcast_mut::<S>())
    }
}
