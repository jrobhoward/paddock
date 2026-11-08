//! The process functions (pids, call and cast, links, monitors, registered
//! names, timers, exit signals), on an instance's [`InstanceState`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::delivery::{self, Origin};
use crate::instance::Message;
use crate::mailbox;
use crate::message::{CallError, Envelope, ExitReason, Info};
use crate::pid::Pid;
use crate::probe;
use crate::state::InstanceState;

/// Exit reasons (`crashed`) for an instance over a host-object limit.
const TIMER_LIMIT: &str = "timer_limit";
const MONITOR_LIMIT: &str = "monitor_limit";
const LINK_LIMIT: &str = "link_limit";

/// A host-object limit as a count.
pub(crate) fn limit(max: u32) -> usize {
    usize::try_from(max).unwrap_or(usize::MAX)
}

/// An instance's pending `send-after` timers. Dropping the set (when the
/// instance exits) cancels them.
#[derive(Debug, Default)]
pub(crate) struct Timers {
    next: u64,
    active: HashMap<u64, AbortHandle>,
}

impl Timers {
    /// Whether one more timer fits under `max`. Timers whose message was
    /// dropped (a full mailbox) never fire into `fired`; they are pruned
    /// here.
    fn has_room(&mut self, max: usize) -> bool {
        if self.active.len() >= max {
            self.active.retain(|_, task| !task.is_finished());
        }
        self.active.len() < max
    }

    fn start(
        &mut self,
        pid: Pid,
        delay: Duration,
        msg: Envelope,
        mailbox: mailbox::WeakSender,
        dropped: Arc<AtomicU64>,
    ) -> u64 {
        self.next += 1;
        let reference = self.next;
        let task = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let Some(mailbox) = mailbox.upgrade() else {
                return;
            };
            let info = Message::Info(Info::Timer { reference, msg });
            if let Err(mpsc::error::TrySendError::Full(_)) = mailbox.try_send(info) {
                dropped.fetch_add(1, Ordering::Relaxed);
                probe::mailbox_drop(pid, "timer");
            }
        });
        self.active.insert(reference, task.abort_handle());
        reference
    }

    /// Cancel a timer. A timer that already fired may still be waiting in the
    /// mailbox and is delivered.
    fn cancel(&mut self, reference: u64) {
        if let Some(task) = self.active.remove(&reference) {
            task.abort();
        }
    }

    /// Forget a timer whose message has been delivered.
    pub(crate) fn fired(&mut self, reference: u64) {
        self.active.remove(&reference);
    }
}

impl Drop for Timers {
    fn drop(&mut self) {
        for task in self.active.values() {
            task.abort();
        }
    }
}

impl InstanceState {
    /// Send a cast from this instance. The error says why it was dropped
    /// ([`delivery::cast`]).
    pub(crate) fn try_cast(&self, to: Pid, msg: Envelope) -> Result<(), CallError> {
        let sent = delivery::cast(&self.ctx.rt, Origin::Local(&self.ctx.me), to, msg);
        if let Err(err) = &sent {
            tracing::debug!(%to, error = %err, "cast dropped");
        }
        sent
    }

    /// Send a cast from this instance, waiting up to `timeout` for room in
    /// the target's full mailbox ([`delivery::cast_wait`]): a native app's
    /// `cast_timeout`. The time waited is counted for the app.
    pub(crate) async fn cast_wait(
        &mut self,
        to: Pid,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<(), CallError> {
        let started = std::time::Instant::now();
        let sent =
            delivery::cast_wait(&self.ctx.rt, Origin::Local(&self.ctx.me), to, msg, timeout).await;
        let waited = started.elapsed();
        self.ctx.stats.cast_waited(waited);
        probe::cast_waited(self.pid, to, waited, &sent);
        if let Err(err) = &sent {
            tracing::debug!(%to, error = %err, waited_us = waited.as_micros(), "waited cast not queued");
        }
        sent
    }
}

/// A registration refused ([`InstanceState::register_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegisterRefused {
    /// The manifest does not list the name.
    AccessDenied,
    /// Another instance holds it.
    Taken(Pid),
}

/// The process functions a native app's requests run. An `Err(String)` is
/// a fault that ends the instance: over a host-object limit, or a call to
/// itself.
impl InstanceState {
    /// This instance's pid.
    pub(crate) fn self_pid(&self) -> Pid {
        self.pid
    }

    /// Call `to` and wait up to `timeout` for the reply.
    pub(crate) async fn call_to(
        &mut self,
        to: Pid,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Result<Envelope, CallError>, String> {
        if to == self.pid {
            // The call would wait on this instance's own mailbox, which it
            // cannot read until this callback returns.
            return Err("call to self would deadlock".into());
        }
        Ok(delivery::call(&self.ctx.rt, Origin::Local(&self.ctx.me), to, msg, timeout).await)
    }

    /// Reply to the call `call_ref`; nothing for an unknown reference (a
    /// duplicate reply, or the caller gave up).
    pub(crate) fn reply_to(&mut self, call_ref: u64, msg: Envelope) {
        if let Some(waiting) = self.take_reply(call_ref) {
            let _ = waiting.send(msg);
        }
    }

    /// Link to `other`.
    pub(crate) fn link_to(&mut self, other: Pid) -> Result<(), String> {
        if other == self.pid {
            return Ok(());
        }
        let max = limit(self.host_objects.links);
        if !self.ctx.rt.links.may_link(self.pid, other, max) {
            return Err(LINK_LIMIT.into());
        }
        if let Err(reason) = delivery::link(&self.ctx.rt, &self.ctx.me, other) {
            // Linking to a dead or unreachable pid is an immediate exit
            // signal (`noproc` or `noconnection`).
            self.ctx.me.exit_signal(other, reason, false);
        }
        Ok(())
    }

    /// Remove a link to `other`.
    pub(crate) fn unlink_from(&mut self, other: Pid) {
        delivery::unlink(&self.ctx.rt, self.pid, other);
    }

    /// Trap exit signals as messages, or not.
    pub(crate) fn set_trapping_exits(&mut self, on: bool) {
        self.ctx.me.set_trap_exits(on);
    }

    /// Monitor `other`: its reference.
    pub(crate) fn monitor_pid(&mut self, other: Pid) -> Result<u64, String> {
        if self.ctx.rt.links.monitors_held(self.pid) >= limit(self.host_objects.monitors) {
            return Err(MONITOR_LIMIT.into());
        }
        let (reference, down) = delivery::monitor(&self.ctx.rt, self.pid, other);
        if let Some(reason) = down {
            self.ctx.me.signal(Info::Down {
                reference,
                pid: other,
                reason,
            });
        }
        Ok(reference)
    }

    /// Remove the monitor `reference`.
    pub(crate) fn demonitor_ref(&mut self, reference: u64) {
        delivery::demonitor(&self.ctx.rt, self.pid, reference);
    }

    /// Register this instance as `name`, which the manifest must list.
    pub(crate) fn register_name(&mut self, name: &str) -> Result<(), RegisterRefused> {
        if !self.registered_names.iter().any(|n| n == name) {
            return Err(RegisterRefused::AccessDenied);
        }
        self.ctx
            .rt
            .registry
            .register(name, self.pid)
            .map_err(RegisterRefused::Taken)
    }

    /// The pid registered as `name`, on this node or another.
    pub(crate) fn whereis_name(&self, name: &str) -> Option<Pid> {
        delivery::whereis(&self.ctx.rt, name)
    }

    /// Send `msg` to this instance after `ms`: the timer's reference.
    pub(crate) fn send_after_ms(&mut self, ms: u32, msg: Envelope) -> Result<u64, String> {
        if !self.timers.has_room(limit(self.host_objects.timers)) {
            return Err(TIMER_LIMIT.into());
        }
        Ok(self.timers.start(
            self.pid,
            Duration::from_millis(ms.into()),
            msg,
            self.ctx.mailbox.clone(),
            self.ctx.dropped.clone(),
        ))
    }

    /// Cancel the timer `reference`.
    pub(crate) fn cancel_timer_ref(&mut self, reference: u64) {
        self.timers.cancel(reference);
    }

    /// Send an exit signal to `other` (to this instance too, as OTP does).
    pub(crate) fn exit_to(&mut self, other: Pid, reason: ExitReason) {
        let untrappable = reason == ExitReason::Killed;
        if other == self.pid {
            // As in OTP, an exit signal to oneself stops a non-trapping
            // instance even with reason `normal`.
            if untrappable || !self.ctx.me.traps_exits() {
                self.ctx.me.exit_self(reason);
            } else {
                self.ctx.me.signal(Info::Exit {
                    from: self.pid,
                    reason,
                });
            }
        } else {
            delivery::exit_signal(
                &self.ctx.rt,
                Origin::Local(&self.ctx.me),
                other,
                reason,
                untrappable,
            );
        }
    }
}
