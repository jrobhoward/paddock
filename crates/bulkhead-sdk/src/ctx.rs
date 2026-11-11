//! The process context: what an app can do besides handling its message.

use std::time::Duration;

use bulkhead_native::app::Ctx as Node;
use bulkhead_native::protocol as p;

use crate::envelope::Envelope;
use crate::types::{
    CallError, CallRef, CastError, ExitReason, MonitorRef, Pid, RegisterError, TimerRef,
};

/// Messaging, links, monitors, timers and the registry, passed to every
/// callback. Each method is one request to the node.
#[derive(Debug)]
pub struct Ctx {
    _private: (),
}

impl Ctx {
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }

    /// This instance's pid.
    #[must_use]
    pub fn pid(&self) -> Pid {
        Node::current().self_pid().into()
    }

    /// Send without waiting. At most once; a message to a dead pid is
    /// dropped silently.
    pub fn cast(&mut self, to: Pid, msg: Envelope) {
        Node::current().cast(to.into(), msg.into());
    }

    /// [`Ctx::cast`], saying whether the message was queued: in the
    /// target's mailbox, or, for an instance on another node, on the link
    /// to that node (not yet in its mailbox). It costs what a cast does.
    ///
    /// # Errors
    ///
    /// [`CastError`] for a dead pid, an unreachable node, a full mailbox
    /// or link, an oversized body, or a messaging ACL that refuses it.
    pub fn try_cast(&mut self, to: Pid, msg: Envelope) -> Result<(), CastError> {
        Node::current()
            .try_cast(to.into(), msg.into())
            .map_err(Into::into)
    }

    /// [`Ctx::try_cast`] that waits up to `timeout` for room if the
    /// target's mailbox is full, so `Ok` means the message is in its
    /// mailbox (not yet handled: a target that crashes loses its mailbox).
    /// This instance handles no other message meanwhile; the wait does not
    /// count against its callback deadline, and the node caps it at that
    /// deadline. A cast to this instance's own pid never waits. To an
    /// instance on another node, the wait happens there and `Ok` means
    /// that node queued it; a timeout is that node's answer unless the
    /// answer itself is late, in which case the cast may still land.
    ///
    /// # Errors
    ///
    /// [`CastError::Timeout`] if no room came in time, and otherwise as
    /// [`Ctx::try_cast`]; to another node, [`CastError::Noconnection`] also
    /// when the session ends while it waits or the node is one release
    /// older and cannot take a waited cast.
    pub fn cast_timeout(
        &mut self,
        to: Pid,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<(), CastError> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        Node::current()
            .cast_timeout(to.into(), msg.into(), ms)
            .map_err(Into::into)
    }

    /// Send and wait for the reply. This instance handles no other message
    /// meanwhile; the time spent waiting does not count against its
    /// callback deadline.
    ///
    /// # Errors
    ///
    /// [`CallError`] for a dead pid, a timeout, a full mailbox, an oversized
    /// body, or a target that exits first. A call to this instance's own pid
    /// crashes the instance instead: it could never be answered.
    pub fn call(
        &mut self,
        to: Pid,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, CallError> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        Node::current()
            .call(to.into(), msg.into(), ms)
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Answer a call that returned [`Reply::Later`](crate::Reply::Later).
    /// A second answer to the same call is dropped.
    pub fn reply(&mut self, call: CallRef, msg: Envelope) {
        Node::current().reply(call.0, msg.into());
    }

    /// Link with another instance.
    pub fn link(&mut self, other: Pid) {
        Node::current().link(other.into());
    }

    /// Remove a link.
    pub fn unlink(&mut self, other: Pid) {
        Node::current().unlink(other.into());
    }

    /// When on, linked exits arrive as [`Info::Exit`](crate::Info::Exit)
    /// instead of stopping this instance. Set it in `init` to cover the
    /// whole life of the instance.
    pub fn trap_exits(&mut self, on: bool) {
        Node::current().trap_exits(on);
    }

    /// Watch another instance; its exit arrives as
    /// [`Info::Down`](crate::Info::Down).
    pub fn monitor(&mut self, other: Pid) -> MonitorRef {
        MonitorRef(Node::current().monitor(other.into()))
    }

    /// Stop watching.
    pub fn demonitor(&mut self, monitor: MonitorRef) {
        Node::current().demonitor(monitor.0);
    }

    /// Register a node-local name listed in the manifest.
    ///
    /// # Errors
    ///
    /// [`RegisterError`] for a name the manifest does not list or another
    /// instance holds.
    pub fn register(&mut self, name: &str) -> Result<(), RegisterError> {
        Node::current().register(name).map_err(|e| match e {
            p::RegisterError::AccessDenied => RegisterError::AccessDenied,
            p::RegisterError::Taken(pid) => RegisterError::Taken(pid.into()),
        })
    }

    /// Look up a registered name, or an app's name (one of its instances,
    /// round-robin).
    #[must_use]
    pub fn whereis(&self, name: &str) -> Option<Pid> {
        Node::current().whereis(name).map(Into::into)
    }

    /// Deliver `msg` to this instance as [`Info::Timer`](crate::Info::Timer)
    /// after `delay`.
    pub fn send_after(&mut self, delay: Duration, msg: Envelope) -> TimerRef {
        let ms = u32::try_from(delay.as_millis()).unwrap_or(u32::MAX);
        TimerRef(Node::current().send_after(ms, msg.into()))
    }

    /// Cancel a timer that has not fired.
    pub fn cancel_timer(&mut self, timer: TimerRef) {
        Node::current().cancel_timer(timer.0);
    }

    /// Send an exit signal. `Killed` cannot be trapped; `Normal` to another
    /// non-trapping instance is ignored.
    pub fn exit(&mut self, other: Pid, reason: &ExitReason) {
        Node::current().exit(other.into(), reason.into());
    }
}
