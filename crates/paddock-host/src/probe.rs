//! Probe points on the message path, for tracers attached to a running
//! node (`docs/OBSERVABILITY.md`).
//!
//! Host code calls the functions here, never a backend directly. With the
//! `probes` feature they fire anyprobe probes under the provider `paddock`:
//! SystemTap SDT on Linux (bpftrace, perf), DTrace USDT on macOS and FreeBSD,
//! ETW on Windows. With no tracer attached a probe costs one enabled check,
//! and its arguments are not computed. Without the feature every function
//! here is empty.
//!
//! An instance is identified by its pid's `id` (node-local, kept across
//! restarts) and, where it matters, its `incarnation`. A sender id of 0 is
//! a message from outside any instance (the admin API, the host itself).
//! Probe names and arguments are an interface: tracing scripts depend on
//! them.

use crate::message::{CallError, ExitReason};
use crate::pid::Pid;

#[cfg(feature = "probes")]
anyprobe::probes! {
    provider = "paddock";

    /// A call was sent: call id (pairs it with `call__done`), sender id,
    /// target id, body bytes.
    pub fn call__start(call: u64, from: u64, to: u64, bytes: u64);
    /// A call finished: call id, target id, outcome (`ok`, `timeout`,
    /// `noproc`, `overloaded`, `too-large`, `access-denied`, `exited`,
    /// `noconnection`).
    pub fn call__done(call: u64, to: u64, outcome: &str);
    /// A cast was sent: sender id, target id, body bytes, outcome (`ok`, or
    /// why it was not queued).
    pub fn cast(from: u64, to: u64, bytes: u64, outcome: &str);
    /// A callback started: instance id, kind (`call`, `cast`, `info`).
    pub fn callback__start(pid: u64, kind: &str);
    /// A callback finished: instance id, kind, guest execution time in
    /// nanoseconds (host time excluded), whether it succeeded.
    pub fn callback__done(pid: u64, kind: &str, guest_ns: u64, ok: bool);
    /// An instance exited: id, incarnation, exit reason.
    pub fn instance__exit(pid: u64, incarnation: u32, reason: &str);
    /// A supervisor restarted a child: the new instance's id and
    /// incarnation, the child's id in its supervisor.
    pub fn supervisor__restart(pid: u64, incarnation: u32, child: &str);
    /// An app rollout started: app, target version.
    pub fn upgrade__start(app: &str, to: &str);
    /// An app rollout finished: app, target version, whether it completed
    /// (`false`: stopped and reverted, or refused).
    pub fn upgrade__done(app: &str, to: &str, ok: bool);
    /// A message was dropped because the mailbox was full: instance id,
    /// kind (`cast`, `timer`).
    pub fn mailbox__drop(pid: u64, kind: &str);
    /// A listener accepted a connection and started its handler: the
    /// handler's instance id, the listener's name. Its end is
    /// `instance__exit`.
    pub fn connection__open(pid: u64, listener: &str);
    /// The session with another node ended (`nodedown`): the node, the
    /// session number that follows.
    pub fn node__down(node: u32, session: u64);
}

/// The outcome name a probe reports for a call or cast.
#[cfg_attr(not(feature = "probes"), allow(dead_code))]
fn outcome<T>(result: &Result<T, CallError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(CallError::Noproc) => "noproc",
        Err(CallError::Noconnection) => "noconnection",
        Err(CallError::Timeout) => "timeout",
        Err(CallError::Overloaded) => "overloaded",
        Err(CallError::TooLarge) => "too-large",
        Err(CallError::AccessDenied) => "access-denied",
        Err(CallError::Exited(_)) => "exited",
    }
}

/// A call is about to be queued. Returns the call id for [`call_done`]; 0
/// when no tracer is attached.
#[inline]
#[allow(unused_variables)]
pub(crate) fn call_start(from: Option<Pid>, to: Pid, bytes: usize) -> u64 {
    #[cfg(feature = "probes")]
    if call__start::enabled() {
        let call = anyprobe::next_id();
        call__start::fire(call, from.map_or(0, |p| p.id), to.id, bytes as u64);
        return call;
    }
    0
}

/// A call finished.
#[inline]
#[allow(unused_variables)]
pub(crate) fn call_done<T>(call: u64, to: Pid, result: &Result<T, CallError>) {
    #[cfg(feature = "probes")]
    anyprobe::fire!(call__done(call, to.id, outcome(result)));
}

/// A cast was offered to a mailbox.
#[inline]
#[allow(unused_variables)]
pub(crate) fn cast_sent(from: Option<Pid>, to: Pid, bytes: usize, result: &Result<(), CallError>) {
    #[cfg(feature = "probes")]
    anyprobe::fire!(cast(
        from.map_or(0, |p| p.id),
        to.id,
        bytes as u64,
        outcome(result)
    ));
}

/// A callback is starting.
#[inline]
#[allow(unused_variables)]
pub(crate) fn callback_start(pid: Pid, kind: &str) {
    #[cfg(feature = "probes")]
    anyprobe::fire!(callback__start(pid.id, kind));
}

/// Whether anyone listens for [`callback_done`], so callers can skip
/// measuring the guest time.
#[inline]
#[must_use]
pub(crate) fn callback_done_enabled() -> bool {
    #[cfg(feature = "probes")]
    return callback__done::enabled();
    #[cfg(not(feature = "probes"))]
    false
}

/// A callback finished after `guest_ns` of guest execution.
#[inline]
#[allow(unused_variables)]
pub(crate) fn callback_done(pid: Pid, kind: &str, guest_ns: u64, ok: bool) {
    #[cfg(feature = "probes")]
    callback__done::fire(pid.id, kind, guest_ns, ok);
}

/// An instance exited.
#[inline]
#[allow(unused_variables)]
pub(crate) fn instance_exit(pid: Pid, reason: &ExitReason) {
    #[cfg(feature = "probes")]
    anyprobe::fire!(instance__exit(pid.id, pid.incarnation, &reason.to_string()));
}

/// A supervisor restarted `child` as `pid`.
#[inline]
#[allow(unused_variables)]
pub(crate) fn supervisor_restart(pid: Pid, child: &str) {
    #[cfg(feature = "probes")]
    anyprobe::fire!(supervisor__restart(pid.id, pid.incarnation, child));
}

/// An app rollout started.
#[inline]
#[allow(unused_variables)]
pub(crate) fn upgrade_start(app: &str, to: &str) {
    #[cfg(feature = "probes")]
    anyprobe::fire!(upgrade__start(app, to));
}

/// An app rollout finished.
#[inline]
#[allow(unused_variables)]
pub(crate) fn upgrade_done(app: &str, to: &str, ok: bool) {
    #[cfg(feature = "probes")]
    anyprobe::fire!(upgrade__done(app, to, ok));
}

/// A listener started a connection handler.
#[inline]
#[allow(unused_variables)]
pub(crate) fn connection_open(pid: Pid, listener: &str) {
    #[cfg(feature = "probes")]
    anyprobe::fire!(connection__open(pid.id, listener));
}

/// A full mailbox dropped a message.
#[inline]
#[allow(unused_variables)]
pub(crate) fn mailbox_drop(pid: Pid, kind: &str) {
    #[cfg(feature = "probes")]
    anyprobe::fire!(mailbox__drop(pid.id, kind));
}

/// The session with another node ended.
#[inline]
#[allow(unused_variables)]
pub(crate) fn node_down(node: u32, session: u64) {
    #[cfg(feature = "probes")]
    anyprobe::fire!(node__down(node, session));
}
