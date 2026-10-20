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
//! ([`crate::services`]): fail softly when not granted, and read named
//! resources from the scope rather than host paths. Time spent in their
//! host functions is host time without their doing anything.
//!
//! An extension that works on its own schedule (polling devices, serving a
//! port) does it from [`Extension::start`], which runs once the node's apps
//! are up: it gets its node configuration (`[extensions."<package>"]`) and
//! an [`ExtensionNode`] to send messages to apps, and stops its tasks when
//! [`ExtensionNode::stopped`] completes. Work that only one node of an HA
//! pair should do (polling a device whose values the pair replicates)
//! checks [`ExtensionNode::runs_singletons`] first.

use std::any::{Any, TypeId};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Arc;

use tokio::sync::watch;
use wasmtime::component::{HasData, Linker};

use super::ServiceCtx;
use crate::apps::Apps;
use crate::error::HostError;
use crate::failover::RunUntil;
use crate::message::{CallError, Envelope};
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

    /// Start node-level work: called once, after the node's apps start,
    /// with the extension's node configuration. Tasks it spawns end when
    /// [`ExtensionNode::stopped`] completes. Nothing by default.
    ///
    /// # Errors
    ///
    /// A message if the configuration is invalid; the node then refuses to
    /// start.
    fn start(&self, node: ExtensionNode) -> Result<(), String> {
        let _ = node;
        Ok(())
    }
}

/// What an extension's node-level work reaches: its configuration, the
/// node's apps, the node's failover lease, and a stop signal. Cheap to
/// clone.
#[derive(Debug, Clone)]
pub struct ExtensionNode {
    config: serde_json::Value,
    apps: Apps,
    lease: watch::Receiver<RunUntil>,
    stop: watch::Receiver<bool>,
}

impl ExtensionNode {
    /// The extension's node configuration (null if the node has none).
    #[must_use]
    pub fn config(&self) -> &serde_json::Value {
        &self.config
    }

    /// Cast `msg` to the app named `app`, picking an instance by the app's
    /// `instances.routing` (a `hash:<field>` pool keeps each key on one
    /// instance).
    ///
    /// # Errors
    ///
    /// [`CallError::Noproc`] if no such app runs, and the target's errors
    /// (`overloaded` for a full mailbox, `too-large`).
    pub fn cast(&self, app: &str, msg: Envelope) -> Result<(), CallError> {
        self.apps.app(app).ok_or(CallError::Noproc)?.cast(msg)
    }

    /// Whether singletons may run on this node now: always on a node
    /// without failover, and on an HA pair's primary while its lease holds
    /// (until its end less the margin, so the two nodes never both answer
    /// yes). Not true on the standby, or while a node starts or steps down.
    /// Read it each time; a lease can end without a change being sent.
    #[must_use]
    pub fn runs_singletons(&self) -> bool {
        self.lease.borrow().allows(tokio::time::Instant::now())
    }

    /// Completes when the node stops; end the extension's tasks then.
    pub async fn stopped(&self) {
        let mut stop = self.stop.clone();
        let _ = stop.wait_for(|stopped| *stopped).await;
    }

    /// Whether the node has stopped.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        *self.stop.borrow()
    }
}

/// Started extensions; [`StartedExtensions::stop`] (or dropping it) tells
/// their tasks to end.
#[derive(Debug)]
pub struct StartedExtensions {
    stop: watch::Sender<bool>,
}

impl StartedExtensions {
    /// Tell every extension's tasks to end.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }
}

impl Drop for StartedExtensions {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start every extension with its entry in `configs` (by package).
pub(crate) fn start(
    extensions: &[Arc<dyn Extension>],
    apps: &Apps,
    lease: &watch::Receiver<RunUntil>,
    configs: &BTreeMap<String, serde_json::Value>,
) -> Result<StartedExtensions, HostError> {
    if let Some(unknown) = configs
        .keys()
        .find(|k| !extensions.iter().any(|e| e.package() == k.as_str()))
    {
        return Err(HostError::InvalidApps(format!(
            "configuration for extension '{unknown}', which this node does not have"
        )));
    }
    let (stop, stopped) = watch::channel(false);
    for ext in extensions {
        let node = ExtensionNode {
            config: configs
                .get(ext.package())
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            apps: apps.clone(),
            lease: lease.clone(),
            stop: stopped.clone(),
        };
        ext.start(node)
            .map_err(|e| HostError::InvalidApps(format!("extension {}: {e}", ext.package())))?;
    }
    Ok(StartedExtensions { stop })
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
