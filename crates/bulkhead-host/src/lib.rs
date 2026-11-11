//! The Bulkhead host runtime: instances that each run a native process (or,
//! for tests, an in-process app) with resource limits and a bounded mailbox,
//! driven one callback at a time by a tokio task.
//!
//! ```no_run
//! use std::time::Duration;
//! use bulkhead_bundle::manifest::NativeProgram;
//! use bulkhead_host::{Envelope, InstanceSpec, NativeCode, Runtime, RuntimeConfig};
//!
//! # async fn demo(spec: InstanceSpec) -> Result<(), Box<dyn std::error::Error>> {
//! let runtime = Runtime::new(RuntimeConfig::default())?;
//! let app = runtime.load_native(NativeCode {
//!     path: "/usr/local/lib/orders".into(),
//!     kind: NativeProgram::Executable,
//! });
//! let instance = runtime.spawn(&app, spec)?;
//! let reply = instance
//!     .call(Envelope::new("text/plain", "ping"), Duration::from_secs(1))
//!     .await?;
//! instance.shutdown(bulkhead_host::ExitReason::Shutdown);
//! let reason = instance.wait().await;
//! # Ok(()) }
//! ```

pub mod acl;
mod apps;
mod config;
mod credit;
mod delivery;
mod error;
pub mod failover;
mod formats;
mod handover;
pub mod health;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "test-apps")]
pub mod in_process;
mod instance;
mod lifecycle;
mod links;
pub mod listener;
mod mailbox;
mod message;
pub mod metrics;
mod native;
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
pub use bulkhead_bundle::manifest::{RestartType, Strategy};
/// Address networks (`10.0.0.0/24`), for extensions' node configuration.
pub use bulkhead_bundle::manifest::{cidr_contains, parse_cidr};
pub use config::{
    Egress, HostObjectRequest, HostObjects, NativeConfig, RuntimeConfig, SecretSource,
    SharedBucket, SqlDatasource, SqlTls,
};
pub use error::HostError;
pub use formats::{FIRST_FORMATS, FinalizeRefused, Finalized};
pub use handover::Handover;
pub use instance::{InstanceEvent, InstanceHandle, SnapshotError};
pub use lifecycle::LifecycleState;
pub use links::LinkInfo;
pub use listener::{Listener, ListenerSpec, ListenerStats};
pub use message::{CallError, Envelope, ExitReason};
pub use native::NativeCode;
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
