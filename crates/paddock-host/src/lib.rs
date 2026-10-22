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

pub mod acl;
mod apps;
mod bindings;
mod compiled;
mod config;
mod delivery;
mod deny;
mod error;
pub mod failover;
mod formats;
mod handover;
pub mod health;
#[cfg(feature = "http")]
pub mod http;
mod instance;
mod lifecycle;
mod links;
pub mod listener;
mod message;
pub mod metrics;
mod output;
pub mod pid;
mod pool;
mod probe;
mod process;
mod registry;
mod replication;
mod runtime;
pub mod services;
mod spec;
mod state;
mod supervisor;
mod table;
pub mod transport;
mod upgrade;

pub use apps::{
    AppDeployment, AppHandle, AppStatus, Apps, NewSideInfo, UpgradeOptions, UpgradeReport,
};
pub use config::{
    Egress, HostObjectRequest, HostObjects, MemoryProfile, RuntimeConfig, SecretSource,
    SharedBucket, SqlDatasource, SqlTls,
};
pub use error::HostError;
pub use formats::{FIRST_FORMATS, FinalizeRefused, Finalized};
pub use handover::Handover;
pub use instance::{InstanceEvent, InstanceHandle, SnapshotError};
pub use lifecycle::LifecycleState;
pub use links::LinkInfo;
pub use listener::{CONNECTION_WORLD, Listener, ListenerSpec, ListenerStats, LoadedConnection};
pub use message::{CallError, Envelope, ExitReason};
pub use paddock_bundle::manifest::{RestartType, Strategy};
/// Address networks (`10.0.0.0/24`), for extensions' node configuration.
pub use paddock_bundle::manifest::{cidr_contains, parse_cidr};
pub use pid::Pid;
/// Hash routing's key reader, for the fuzz targets in `fuzz/` only.
#[cfg(fuzzing)]
#[doc(hidden)]
pub use pool::routing_key;
pub use replication::ReplicationStatus;
pub use runtime::{LoadedApp, Runtime};
pub use spec::{InstanceSpec, PRIV_PATH, Preopen};
pub use supervisor::{
    Backoff, Child, ChildHandle, ChildSpec, ChildStart, SupervisorHandle, SupervisorSpec,
};
pub use transport::membership::{Member, Role};
pub use upgrade::{UpgradeError, validate_blue_green, validate_upgrade};
/// The wasmtime this crate links against, for extensions' bindings
/// ([`services::extension`]).
pub use wasmtime;
