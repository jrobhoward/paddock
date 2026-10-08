//! Pids, references, exit reasons, errors and info messages.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::bindings::paddock::runtime::types as wit;
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

impl From<wit::Pid> for Pid {
    fn from(p: wit::Pid) -> Self {
        Self {
            node: p.node,
            id: p.id,
            incarnation: p.incarnation,
        }
    }
}

impl From<Pid> for wit::Pid {
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

impl From<wit::ExitReason> for ExitReason {
    fn from(r: wit::ExitReason) -> Self {
        match r {
            wit::ExitReason::Normal => Self::Normal,
            wit::ExitReason::Shutdown => Self::Shutdown,
            wit::ExitReason::Killed => Self::Killed,
            wit::ExitReason::Noproc => Self::Noproc,
            wit::ExitReason::Noconnection => Self::Noconnection,
            wit::ExitReason::Timeout => Self::Timeout,
            wit::ExitReason::Crashed(why) => Self::Crashed(why),
        }
    }
}

impl From<&ExitReason> for wit::ExitReason {
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

impl From<wit::CallError> for CallError {
    fn from(e: wit::CallError) -> Self {
        match e {
            wit::CallError::Noproc => Self::Noproc,
            wit::CallError::Noconnection => Self::Noconnection,
            wit::CallError::Timeout => Self::Timeout,
            wit::CallError::Overloaded => Self::Overloaded,
            wit::CallError::TooLarge => Self::TooLarge,
            wit::CallError::AccessDenied => Self::AccessDenied,
            wit::CallError::Exited(reason) => Self::Exited(reason.into()),
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

impl From<wit::Info> for Info {
    fn from(info: wit::Info) -> Self {
        match info {
            wit::Info::Exit((from, reason)) => Self::Exit {
                from: from.into(),
                reason: reason.into(),
            },
            wit::Info::Down((monitor, pid, reason)) => Self::Down {
                monitor: MonitorRef(monitor),
                pid: pid.into(),
                reason: reason.into(),
            },
            wit::Info::Timer((timer, msg)) => Self::Timer {
                timer: TimerRef(timer),
                msg: msg.into(),
            },
            wit::Info::HostEvent((name, msg)) => Self::Event {
                name,
                msg: msg.into(),
            },
        }
    }
}
