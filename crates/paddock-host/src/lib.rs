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
mod error;
mod instance;
mod lifecycle;
mod links;
mod message;
mod pid;
mod pool;
mod registry;
mod runtime;
mod spec;
mod state;
mod supervisor;
mod table;

pub use apps::{AppDeployment, AppHandle, AppStatus, Apps};
pub use config::{MemoryProfile, RuntimeConfig};
pub use error::HostError;
pub use instance::InstanceHandle;
pub use lifecycle::LifecycleState;
pub use message::{CallError, Envelope, ExitReason};
pub use paddock_manifest::{RestartType, Strategy};
pub use pid::Pid;
pub use runtime::{LoadedApp, Runtime};
pub use spec::InstanceSpec;
pub use supervisor::{
    Backoff, Child, ChildHandle, ChildSpec, ChildStart, SupervisorHandle, SupervisorSpec,
};
