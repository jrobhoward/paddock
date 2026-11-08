//! Host services from outside `bulkhead-host`: extensions.
//!
//! An extension serves one package outside the reserved `wasi` and
//! `bulkhead` namespaces (`example:greeting`). Apps declare it under
//! `capabilities.extensions."<package>"` with a scope table the extension
//! reads, and the operator grants it by package name in the policy's
//! `grant` lists, like a built-in capability.
//!
//! An extension crate depends on `bulkhead-host` alone. Native apps reach
//! it through [`Extension::call`]: a function named by a string
//! (`greet#hello` by convention), with its arguments and result in CBOR.
//! It decodes the arguments with [`NativeCall::args`], reaches its
//! per-instance state with [`extension_ctx`], and encodes the result with
//! [`reply`]. Its state comes from [`Extension::instance`], where
//! [`ExtensionApp::granted`] says whether to answer access-denied.
//!
//! Extensions follow the rules of the built-in services
//! ([`crate::services`]): fail softly when not granted, and read named
//! resources from the scope rather than host paths.
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
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;

use tokio::sync::watch;

use super::ServiceCtx;
use crate::apps::Apps;
use crate::error::HostError;
use crate::failover::RunUntil;
use crate::message::{CallError, Envelope};
use crate::state::InstanceState;

/// The node's data for an instance. Opaque: an extension reaches its own
/// state through [`extension_ctx`].
pub type StoreData = InstanceState;

/// A host service for one package, provided from outside
/// `bulkhead-host`. See the module documentation.
pub trait Extension: Send + Sync + 'static {
    /// The package served, `namespace:package` (`example:greeting`): the
    /// key apps declare under `capabilities.extensions` and operators grant.
    fn package(&self) -> &str;

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

    /// Serve an app's call of a function (see the module documentation),
    /// with the calling instance's data: reach this extension's state with
    /// [`extension_ctx`]. [`NativeCall::no_such_function`] for a function
    /// it does not serve.
    fn call<'a>(&'a self, data: &'a mut StoreData, call: NativeCall) -> NativeReply<'a>;
}

/// What [`Extension::call`] returns: the result in CBOR, or why there is
/// none.
pub type NativeReply<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<u8>, NativeCallError>> + Send + 'a>>;

/// A native app's call of an extension's function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCall {
    function: String,
    args: Vec<u8>,
}

impl NativeCall {
    /// A call of `function` with `args` in CBOR.
    #[must_use]
    pub fn new(function: impl Into<String>, args: Vec<u8>) -> Self {
        Self {
            function: function.into(),
            args,
        }
    }

    /// The function's name (`greet#hello`).
    #[must_use]
    pub fn function(&self) -> &str {
        &self.function
    }

    /// The arguments as `T`: by convention a tuple of the WIT parameters.
    ///
    /// # Errors
    ///
    /// [`NativeCallError::BadArguments`] if they do not decode as `T`.
    pub fn args<T: DeserializeOwned>(&self) -> Result<T, NativeCallError> {
        ciborium::from_reader(self.args.as_slice())
            .map_err(|e| NativeCallError::BadArguments(format!("{}: {e}", self.function)))
    }

    /// The error for a function this extension does not serve.
    #[must_use]
    pub fn no_such_function(&self) -> NativeCallError {
        NativeCallError::NoSuchFunction(self.function.clone())
    }
}

/// `value` in CBOR, as [`Extension::call`] returns it.
///
/// # Errors
///
/// [`NativeCallError::Crash`] if `value` does not encode (a map with keys
/// that are not strings, for one).
pub fn reply<T: Serialize>(value: &T) -> Result<Vec<u8>, NativeCallError> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out)
        .map_err(|e| NativeCallError::Crash(format!("encoding a reply: {e}")))?;
    Ok(out)
}

/// Why [`Extension::call`] has no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeCallError {
    /// The extension serves no such function to native apps; the app gets
    /// an error.
    NoSuchFunction(String),
    /// The arguments are not what the function takes; the app gets an
    /// error.
    BadArguments(String),
    /// The calling instance crashes with this reason.
    Crash(String),
}

impl std::fmt::Display for NativeCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchFunction(name) => write!(f, "no function {name} for native apps"),
            Self::BadArguments(why) => write!(f, "bad arguments to {why}"),
            Self::Crash(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for NativeCallError {}

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

/// The context an extension's function runs with: its state of type `S`
/// and the instance's shared facilities.
///
/// # Panics
///
/// If no extension created state of type `S` for this instance, which
/// means `call` and `instance` of one extension disagree about the type.
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
