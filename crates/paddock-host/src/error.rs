//! Errors from the runtime.

use thiserror::Error;

/// A runtime operation that failed. Failures inside an instance are not
/// errors: they end the instance with an [`ExitReason`](crate::ExitReason).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HostError {
    /// The engine could not be created from the configuration.
    #[error("cannot create engine: {0}")]
    Engine(String),

    /// The component failed to compile or to link against the host.
    #[error("cannot load component: {0}")]
    Load(String),

    /// Every instance slot is in use.
    #[error("instance limit reached ({0} instances)")]
    InstanceLimit(u32),

    /// The instance asks for more memory than the node's memory profile
    /// reserves.
    #[error("memory_max of {requested} bytes exceeds the node limit of {limit} bytes")]
    MemoryAboveProfile {
        /// The instance's `memory_max`.
        requested: u64,
        /// The memory profile's limit.
        limit: u64,
    },

    /// The instance asks for more host objects than the node allows
    /// ([`HostObjects`](crate::HostObjects)).
    #[error("{0}")]
    HostObjectsAboveNode(String),

    /// A supervisor spec is invalid, or the supervisor has exited.
    #[error("invalid supervisor: {0}")]
    InvalidSupervisor(String),

    /// A set of apps cannot be started as given.
    #[error("cannot start apps: {0}")]
    InvalidApps(String),

    /// A listener could not bind its address.
    #[error("cannot listen: {0}")]
    Listen(String),

    /// Node storage (the data directory or the keyvalue database) could not
    /// be opened.
    #[error("cannot open storage: {0}")]
    Storage(String),

    /// A preopened directory does not exist.
    #[error("cannot preopen: {0}")]
    Preopen(String),

    /// A respawn named an instance id that is still alive.
    #[error("instance {0} is still running")]
    StillRunning(crate::Pid),
}

/// Render a wasmtime error with its causes on one line.
pub(crate) fn describe(err: &wasmtime::Error) -> String {
    format!("{err:#}")
}
