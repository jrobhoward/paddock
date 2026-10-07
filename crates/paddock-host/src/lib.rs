//! The Paddock host runtime: a wasmtime engine with a pooling allocator, an
//! epoch ticker, and instances that each own a Store, resource limits and a
//! bounded mailbox, driven one callback at a time by a tokio task.
//!
//! ```no_run
//! use std::time::Duration;
//! use paddock_host::{Envelope, InstanceSpec, Runtime, RuntimeConfig};
//!
//! # async fn demo(component: &[u8], spec: InstanceSpec) -> Result<(), Box<dyn std::error::Error>> {
//! let runtime = Runtime::new(RuntimeConfig::default())?;
//! let app = runtime.load(component)?;
//! let instance = runtime.spawn(&app, spec)?;
//! let reply = instance
//!     .call(Envelope::new("text/plain", "ping"), Duration::from_secs(1))
//!     .await?;
//! instance.shutdown(paddock_host::ExitReason::Shutdown);
//! let reason = instance.wait().await;
//! # Ok(()) }
//! ```

mod apps;
mod bindings;
mod config;
mod deny;
mod error;
mod instance;
mod lifecycle;
mod links;
mod message;
mod output;
mod pid;
mod pool;
mod registry;
mod runtime;
pub mod services;
mod spec;
mod state;
mod supervisor;
mod table;
mod upgrade;

pub use apps::{AppDeployment, AppHandle, AppStatus, Apps, UpgradeOptions, UpgradeReport};
pub use config::{MemoryProfile, RuntimeConfig};
pub use error::HostError;
pub use instance::{InstanceHandle, SnapshotError};
pub use lifecycle::LifecycleState;
pub use message::{CallError, Envelope, ExitReason};
pub use paddock_bundle::manifest::{RestartType, Strategy};
pub use pid::Pid;
/// Hash routing's key reader, for the fuzz targets in `fuzz/` only.
#[cfg(fuzzing)]
#[doc(hidden)]
pub use pool::routing_key;
pub use runtime::{LoadedApp, Runtime};
pub use spec::{InstanceSpec, PRIV_PATH, Preopen};
pub use supervisor::{
    Backoff, Child, ChildHandle, ChildSpec, ChildStart, SupervisorHandle, SupervisorSpec,
};
pub use upgrade::{UpgradeError, validate_upgrade};
