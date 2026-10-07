//! Host-side message, exit and error types, and their conversions to the
//! generated guest bindings.

use std::fmt;

use crate::bindings::paddock::runtime::types as wit;
use crate::pid::Pid;

/// A message: opaque bytes plus how to decode them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// How to decode `body` (`application/cbor` by default).
    pub content_type: String,
    /// Schema id from the app's message catalog, if any.
    pub schema: Option<String>,
    /// The encoded message.
    pub body: Vec<u8>,
}

impl Envelope {
    /// An envelope with no schema id.
    #[must_use]
    pub fn new(content_type: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            content_type: content_type.into(),
            schema: None,
            body: body.into(),
        }
    }
}

/// Why an instance stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// `init` returned, or the instance stopped itself, without error.
    Normal,
    /// Stopped by its supervisor or an operator, gracefully.
    Shutdown,
    /// Stopped without running `terminate`.
    Killed,
    /// The pid does not exist.
    Noproc,
    /// The node hosting the pid became unreachable.
    Noconnection,
    /// A callback exceeded its deadline.
    Timeout,
    /// The guest trapped or exceeded a limit; the string says which.
    Crashed(String),
}

impl ExitReason {
    /// Whether this is a normal exit (`normal` or `shutdown`), which does not
    /// propagate through links or count as a failure.
    #[must_use]
    pub fn is_normal(&self) -> bool {
        matches!(self, Self::Normal | Self::Shutdown)
    }
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

/// Why a call or cast failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CallError {
    /// No instance has that pid.
    #[error("noproc")]
    Noproc,
    /// The target's node became unreachable.
    #[error("noconnection")]
    Noconnection,
    /// No reply arrived within the timeout.
    #[error("timeout")]
    Timeout,
    /// The target's mailbox is full.
    #[error("overloaded")]
    Overloaded,
    /// The message body is larger than the target's `message_max_bytes`.
    #[error("too large")]
    TooLarge,
    /// The target's messaging ACL refused the sender.
    #[error("access denied")]
    AccessDenied,
    /// The target exited before replying.
    #[error("exited: {0}")]
    Exited(ExitReason),
}

impl From<Envelope> for wit::Envelope {
    fn from(e: Envelope) -> Self {
        Self {
            content_type: e.content_type,
            schema: e.schema,
            body: e.body,
        }
    }
}

impl From<wit::Envelope> for Envelope {
    fn from(e: wit::Envelope) -> Self {
        Self {
            content_type: e.content_type,
            schema: e.schema,
            body: e.body,
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

impl From<Pid> for wit::Pid {
    fn from(p: Pid) -> Self {
        Self {
            node: p.node,
            id: p.id,
            incarnation: p.incarnation,
        }
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

impl From<&CallError> for wit::CallError {
    fn from(e: &CallError) -> Self {
        match e {
            CallError::Noproc => Self::Noproc,
            CallError::Noconnection => Self::Noconnection,
            CallError::Timeout => Self::Timeout,
            CallError::Overloaded => Self::Overloaded,
            CallError::TooLarge => Self::TooLarge,
            CallError::AccessDenied => Self::AccessDenied,
            CallError::Exited(reason) => Self::Exited(reason.into()),
        }
    }
}

/// Something the host delivers to `handle-info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Info {
    /// A linked instance exited, and this one traps exits; or another
    /// instance sent an exit signal.
    Exit {
        /// Who exited or sent the signal.
        from: Pid,
        /// Why.
        reason: ExitReason,
    },
    /// A monitored instance exited.
    Down {
        /// The reference `monitor` returned.
        reference: u64,
        /// The instance that exited.
        pid: Pid,
        /// Why.
        reason: ExitReason,
    },
    /// A named host event (`host-event`). No Phase 1 service sends one yet;
    /// it is reserved so later events reach `@0.1.0` guests.
    #[allow(dead_code)]
    HostEvent {
        /// The event's name.
        name: String,
        /// Its payload.
        msg: Envelope,
    },
    /// A timer from `send-after` fired.
    Timer {
        /// The reference `send-after` returned.
        reference: u64,
        /// The message it was given.
        msg: Envelope,
    },
}

impl From<Info> for wit::Info {
    fn from(info: Info) -> Self {
        match info {
            Info::Exit { from, reason } => Self::Exit((from.into(), (&reason).into())),
            Info::Down {
                reference,
                pid,
                reason,
            } => Self::Down((reference, pid.into(), (&reason).into())),
            Info::Timer { reference, msg } => Self::Timer((reference, msg.into())),
            Info::HostEvent { name, msg } => Self::HostEvent((name, msg.into())),
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
