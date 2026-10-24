//! Between the native protocol's types and the host's (and the WIT
//! bindings', which the process functions take).

use paddock_native::protocol as p;

use crate::bindings::paddock::runtime::types as wit;
use crate::message::{Envelope, ExitReason, Info, LateEvent};
use crate::pid::Pid;

pub(crate) fn pid(pid: Pid) -> p::Pid {
    p::Pid {
        node: pid.node,
        id: pid.id,
        incarnation: pid.incarnation,
    }
}

pub(crate) fn wit_pid(pid: p::Pid) -> wit::Pid {
    Pid {
        node: pid.node,
        id: pid.id,
        incarnation: pid.incarnation,
    }
    .into()
}

pub(crate) fn from_wit_pid(pid: wit::Pid) -> p::Pid {
    self::pid(Pid::from(pid))
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

pub(crate) fn wit_envelope(env: p::Envelope) -> wit::Envelope {
    host_envelope(env).into()
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

pub(crate) fn call_error(e: wit::CallError) -> p::CallError {
    match e {
        wit::CallError::Noproc => p::CallError::Noproc,
        wit::CallError::Noconnection => p::CallError::Noconnection,
        wit::CallError::Timeout => p::CallError::Timeout,
        wit::CallError::Overloaded => p::CallError::Overloaded,
        wit::CallError::TooLarge => p::CallError::TooLarge,
        wit::CallError::AccessDenied => p::CallError::AccessDenied,
        wit::CallError::Exited(reason) => {
            p::CallError::Exited(exit_reason(&ExitReason::from(reason)))
        }
    }
}

pub(crate) fn register_error(e: wit::RegisterError) -> p::RegisterError {
    match e {
        wit::RegisterError::AccessDenied => p::RegisterError::AccessDenied,
        wit::RegisterError::Taken(holder) => p::RegisterError::Taken(from_wit_pid(holder)),
    }
}
