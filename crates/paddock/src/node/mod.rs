//! The Paddock node daemon: node configuration, the deploy store, the admin
//! API on a Unix socket, and the audit log.
//!
//! A node verifies each bundle against its trusted keys, checks the
//! component's imports against the manifest and the operator policy, keeps
//! the bundle on disk, and runs the app under a root supervisor. On restart
//! it re-verifies and restarts every app that was running.
//!
//! The admin API is described in [`crate::api`]; the `paddock` CLI wraps it, and `paddock node` runs a node.

mod audit;
mod config;
mod daemon;
mod error;
mod logring;
mod run;
mod server;
mod state_view;
mod store;

pub use config::{
    BucketConfig, BucketScope, ClusterConfig, ClusterRole, ListenerBinding, MemoryProfile,
    NodeConfig,
};
pub use daemon::Node;
pub use error::NodeError;
pub use logring::{LogRing, RingLayer};
pub use run::{builtin_extensions, run};
pub use server::{MAX_BODY, serve};
