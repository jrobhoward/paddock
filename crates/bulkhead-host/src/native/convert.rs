//! Between the native protocol's types and the host's.

use bulkhead_native::protocol as p;

use crate::message::{CallError, Envelope, ExitReason, Info, LateEvent};
use crate::pid::Pid;
use crate::process::RegisterRefused;

pub(crate) fn pid(pid: Pid) -> p::Pid {
    p::Pid {
        node: pid.node,
        id: pid.id,
        incarnation: pid.incarnation,
    }
}

pub(crate) fn host_pid(pid: p::Pid) -> Pid {
    Pid {
        node: pid.node,
        id: pid.id,
        incarnation: pid.incarnation,
    }
}

pub(crate) fn envelope(env: Envelope) -> p::Envelope {
    p::Envelope {
        content_type: env.content_type,
        schema: env.schema,
        body: env.body,
    }
}

pub(crate) fn host_envelope(env: p::Envelope) -> Envelope {
    Envelope {
        content_type: env.content_type,
        schema: env.schema,
        body: env.body,
    }
}

pub(crate) fn exit_reason(reason: &ExitReason) -> p::ExitReason {
    match reason {
        ExitReason::Normal => p::ExitReason::Normal,
        ExitReason::Shutdown => p::ExitReason::Shutdown,
        ExitReason::Killed => p::ExitReason::Killed,
        ExitReason::Noproc => p::ExitReason::Noproc,
        ExitReason::Noconnection => p::ExitReason::Noconnection,
        ExitReason::Timeout => p::ExitReason::Timeout,
        ExitReason::Crashed(why) => p::ExitReason::Crashed(why.clone()),
    }
}

pub(crate) fn host_exit_reason(reason: p::ExitReason) -> ExitReason {
    match reason {
        p::ExitReason::Normal => ExitReason::Normal,
        p::ExitReason::Shutdown => ExitReason::Shutdown,
        p::ExitReason::Killed => ExitReason::Killed,
        p::ExitReason::Noproc => ExitReason::Noproc,
        p::ExitReason::Noconnection => ExitReason::Noconnection,
        p::ExitReason::Timeout => ExitReason::Timeout,
        p::ExitReason::Crashed(why) => ExitReason::Crashed(why),
    }
}

/// An info as the app receives it. A late event is taken now; `None` if
/// it was withdrawn.
pub(crate) fn info(info: Info) -> Option<p::Info> {
    Some(match info {
        Info::Exit { from, reason } => p::Info::Exit {
            from: pid(from),
            reason: exit_reason(&reason),
        },
        Info::Down {
            reference,
            pid: down,
            reason,
        } => p::Info::Down {
            reference,
            pid: pid(down),
            reason: exit_reason(&reason),
        },
        Info::Timer { reference, msg } => p::Info::Timer {
            reference,
            msg: envelope(msg),
        },
        Info::HostEvent { name, msg } => p::Info::HostEvent {
            name,
            msg: envelope(msg),
        },
        Info::Late(LateEvent(late)) => {
            let (name, msg) = late.take()?;
            p::Info::HostEvent {
                name,
                msg: envelope(msg),
            }
        }
    })
}

pub(crate) fn call_error(e: &CallError) -> p::CallError {
    match e {
        CallError::Noproc => p::CallError::Noproc,
        CallError::Noconnection => p::CallError::Noconnection,
        CallError::Timeout => p::CallError::Timeout,
        CallError::Overloaded => p::CallError::Overloaded,
        CallError::TooLarge => p::CallError::TooLarge,
        CallError::AccessDenied => p::CallError::AccessDenied,
        CallError::Exited(reason) => p::CallError::Exited(exit_reason(reason)),
    }
}

/// Why a cast was not queued. `timeout` comes only from a cast that
/// waits; `delivery` never answers a cast with `exited`, which belongs to
/// calls, and were it to, the target is gone.
pub(crate) fn cast_error(e: &CallError) -> p::CastError {
    match e {
        CallError::Noproc | CallError::Exited(_) => p::CastError::Noproc,
        CallError::Noconnection => p::CastError::Noconnection,
        CallError::Overloaded => p::CastError::Overloaded,
        CallError::Timeout => p::CastError::Timeout,
        CallError::TooLarge => p::CastError::TooLarge,
        CallError::AccessDenied => p::CastError::AccessDenied,
    }
}

pub(crate) fn register_error(e: RegisterRefused) -> p::RegisterError {
    match e {
        RegisterRefused::AccessDenied => p::RegisterError::AccessDenied,
        RegisterRefused::Taken(holder) => p::RegisterError::Taken(pid(holder)),
    }
}
