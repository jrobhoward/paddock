//! Pids, references, exit reasons, errors and info messages.

use std::fmt;

use bulkhead_native::protocol as p;
use serde::{Deserialize, Serialize};

use crate::envelope::Envelope;

/// Identifies one instance: `{node, id, incarnation}`. A restart keeps the id
/// and bumps the incarnation, so an old pid never reaches the new instance.
/// The id's high 24 bits are the creation of the node's start that made it,
/// and its low 40 a serial number, so pids print as
/// `<node.serial.incarnation@creation>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Pid {
    /// Node.
    pub node: u32,
    /// Instance id.
    pub id: u64,
    /// Incarnation.
    pub incarnation: u32,
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<{}.{}.{}@{}>",
            self.node,
            self.id & ((1 << 40) - 1),
            self.incarnation,
            self.id >> 40
        )
    }
}

impl From<p::Pid> for Pid {
    fn from(p: p::Pid) -> Self {
        Self {
            node: p.node,
            id: p.id,
            incarnation: p.incarnation,
        }
    }
}

impl From<Pid> for p::Pid {
    fn from(p: Pid) -> Self {
        Self {
            node: p.node,
            id: p.id,
            incarnation: p.incarnation,
        }
    }
}

macro_rules! reference {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub u64);
    };
}

reference!(
    /// A call awaiting a reply. Keep it to answer later with
    /// [`Ctx::reply`](crate::Ctx::reply); it is serializable, so it can go
    /// in a snapshot.
    CallRef
);
reference!(
    /// A monitor, from [`Ctx::monitor`](crate::Ctx::monitor).
    MonitorRef
);
reference!(
    /// A timer, from [`Ctx::send_after`](crate::Ctx::send_after).
    TimerRef
);

/// Why an instance stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    /// Without error.
    Normal,
    /// Stopped gracefully by its supervisor or an operator.
    Shutdown,
    /// Stopped without `terminate`.
    Killed,
    /// The pid does not exist.
    Noproc,
    /// Its node became unreachable.
    Noconnection,
    /// A callback exceeded its deadline.
    Timeout,
    /// It trapped or exceeded a limit.
    Crashed(String),
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Normal => f.write_str("normal"),
            Self::Shutdown => f.write_str("shutdown"),
            Self::Killed => f.write_str("killed"),
            Self::Noproc => f.write_str("noproc"),
            Self::Noconnection => f.write_str("noconnection"),
            Self::Timeout => f.write_str("timeout"),
            Self::Crashed(why) => write!(f, "crashed: {why}"),
        }
    }
}

impl From<p::ExitReason> for ExitReason {
    fn from(r: p::ExitReason) -> Self {
        match r {
            p::ExitReason::Normal => Self::Normal,
            p::ExitReason::Shutdown => Self::Shutdown,
            p::ExitReason::Killed => Self::Killed,
            p::ExitReason::Noproc => Self::Noproc,
            p::ExitReason::Noconnection => Self::Noconnection,
            p::ExitReason::Timeout => Self::Timeout,
            p::ExitReason::Crashed(why) => Self::Crashed(why),
        }
    }
}

impl From<&ExitReason> for p::ExitReason {
    fn from(r: &ExitReason) -> Self {
        match r {
            ExitReason::Normal => Self::Normal,
            ExitReason::Shutdown => Self::Shutdown,
            ExitReason::Killed => Self::Killed,
            ExitReason::Noproc => Self::Noproc,
            ExitReason::Noconnection => Self::Noconnection,
            ExitReason::Timeout => Self::Timeout,
            ExitReason::Crashed(why) => Self::Crashed(why.clone()),
        }
    }
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// No instance has that pid.
    Noproc,
    /// Its node is unreachable.
    Noconnection,
    /// No reply in time.
    Timeout,
    /// Its mailbox is full.
    Overloaded,
    /// The body is over its `message_max_bytes`.
    TooLarge,
    /// Its messaging ACL refused the call.
    AccessDenied,
    /// It exited before replying.
    Exited(ExitReason),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Noproc => f.write_str("noproc"),
            Self::Noconnection => f.write_str("noconnection"),
            Self::Timeout => f.write_str("timeout"),
            Self::Overloaded => f.write_str("overloaded"),
            Self::TooLarge => f.write_str("too large"),
            Self::AccessDenied => f.write_str("access denied"),
            Self::Exited(reason) => write!(f, "exited: {reason}"),
        }
    }
}

impl std::error::Error for CallError {}

impl From<p::CallError> for CallError {
    fn from(e: p::CallError) -> Self {
        match e {
            p::CallError::Noproc => Self::Noproc,
            p::CallError::Noconnection => Self::Noconnection,
            p::CallError::Timeout => Self::Timeout,
            p::CallError::Overloaded => Self::Overloaded,
            p::CallError::TooLarge => Self::TooLarge,
            p::CallError::AccessDenied => Self::AccessDenied,
            p::CallError::Exited(reason) => Self::Exited(reason.into()),
        }
    }
}

/// Why a cast was not queued ([`Ctx::try_cast`](crate::Ctx::try_cast),
/// [`Ctx::cast_timeout`](crate::Ctx::cast_timeout)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CastError {
    /// No instance has that pid.
    Noproc,
    /// Its node is unreachable.
    Noconnection,
    /// Its mailbox is full, or the link to its node is over its budget.
    Overloaded,
    /// The body is over its `message_max_bytes`.
    TooLarge,
    /// Its messaging ACL refused the cast.
    AccessDenied,
    /// No room in its mailbox within the timeout (`cast_timeout`); the
    /// message was not queued.
    Timeout,
}

impl fmt::Display for CastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Noproc => "noproc",
            Self::Noconnection => "noconnection",
            Self::Overloaded => "overloaded",
            Self::TooLarge => "too large",
            Self::AccessDenied => "access denied",
            Self::Timeout => "timeout",
        })
    }
}

impl std::error::Error for CastError {}

impl From<p::CastError> for CastError {
    fn from(e: p::CastError) -> Self {
        use p::CastError as P;
        match e {
            P::Noproc => Self::Noproc,
            P::Noconnection => Self::Noconnection,
            P::Overloaded => Self::Overloaded,
            P::TooLarge => Self::TooLarge,
            P::AccessDenied => Self::AccessDenied,
            P::Timeout => Self::Timeout,
        }
    }
}

/// Why `register` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// The manifest's `registered_names` does not list the name.
    AccessDenied,
    /// Another instance holds it.
    Taken(Pid),
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("name not listed in the manifest"),
            Self::Taken(pid) => write!(f, "name held by {pid}"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// Something the host delivers to `handle_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Info {
    /// A linked instance exited (with trap-exits on), or an exit signal
    /// arrived.
    Exit {
        /// Who.
        from: Pid,
        /// Why.
        reason: ExitReason,
    },
    /// A monitored instance exited.
    Down {
        /// The monitor.
        monitor: MonitorRef,
        /// Who.
        pid: Pid,
        /// Why.
        reason: ExitReason,
    },
    /// A timer fired.
    Timer {
        /// The timer.
        timer: TimerRef,
        /// The message it carried.
        msg: Envelope,
    },
    /// A named host event. Ignore names you do not know: later hosts add
    /// events here. The SDK handles `log-level` itself.
    Event {
        /// The event's name.
        name: String,
        /// Its payload.
        msg: Envelope,
    },
}

impl From<p::Info> for Info {
    fn from(info: p::Info) -> Self {
        match info {
            p::Info::Exit { from, reason } => Self::Exit {
                from: from.into(),
                reason: reason.into(),
            },
            p::Info::Down {
                reference,
                pid,
                reason,
            } => Self::Down {
                monitor: MonitorRef(reference),
                pid: pid.into(),
                reason: reason.into(),
            },
            p::Info::Timer { reference, msg } => Self::Timer {
                timer: TimerRef(reference),
                msg: msg.into(),
            },
            p::Info::HostEvent { name, msg } => Self::Event {
                name,
                msg: msg.into(),
            },
            p::Info::KvChanged {
                watch,
                bucket,
                key,
                entry,
                revision,
            } => {
                // With what the node read when it delivered the notice
                // (`watch::Change::current`).
                let (value, revision) = match entry {
                    Some((value, at)) => (Some(value.0), at),
                    None => (None, revision),
                };
                let body = serde_json::json!({
                    "watch": watch,
                    "bucket": bucket,
                    "key": key,
                    "revision": revision,
                    "carried": true,
                    "value": value,
                });
                Self::Event {
                    name: "kv-changed".to_string(),
                    msg: Envelope::new("application/json", body.to_string()),
                }
            }
        }
    }
}
