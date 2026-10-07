//! The Paddock node daemon: node configuration, the deploy store, the admin
//! API on a Unix socket, and the audit log.
//!
//! A node verifies each bundle against its trusted keys, checks the
//! component's imports against the manifest and the operator policy, keeps
//! the bundle on disk, and runs the app under a root supervisor. On restart
//! it re-verifies and restarts every app that was running.
//!
//! The admin API is described in [`paddock_api`]; the `paddock` CLI wraps it.

mod audit;
mod config;
mod error;
mod logring;
mod node;
mod server;
mod store;

pub use config::{MemoryProfile, NodeConfig};
pub use error::NodeError;
pub use logring::{LogRing, RingLayer};
pub use node::Node;
pub use server::{MAX_BODY, serve};
