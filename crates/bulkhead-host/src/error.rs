//! Errors from the runtime.

use thiserror::Error;

/// A runtime operation that failed. Failures inside an instance are not
/// errors: they end the instance with an [`ExitReason`](crate::ExitReason).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HostError {
    /// The runtime cannot start as configured: a creation number out of
    /// range, an extension package refused, a second transport, or node
    /// stores in a format newer than this build reads.
    #[error("cannot start the runtime: {0}")]
    Engine(String),

    /// The process table already holds `max_processes` instances.
    #[error("instance limit reached ({0} instances)")]
    InstanceLimit(u32),

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

    /// HTTP egress could not start: an unreadable CA file.
    #[error("cannot set up HTTP egress: {0}")]
    Egress(String),

    /// A preopened directory does not exist.
    #[error("cannot preopen: {0}")]
    Preopen(String),

    /// A singleton's handover record could not be read, written or
    /// deleted ([`Runtime::take_handover`](crate::Runtime::take_handover)).
    #[error("handover: {0}")]
    Handover(String),

    /// A respawn named an instance id that is still alive.
    #[error("instance {0} is still running")]
    StillRunning(crate::Pid),

    /// Node config `[native] cgroup` is not a cgroup v2 directory delegated
    /// to the node.
    #[error("cannot set up native apps' cgroup: {0}")]
    NativeCgroup(String),
}
