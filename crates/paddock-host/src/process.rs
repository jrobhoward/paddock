//! The `paddock:runtime/process` host functions (pids, call and cast,
//! links, monitors, registered names, timers, exit signals), on an
//! instance's [`InstanceState`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use wasmtime::{Error, Result};

use crate::bindings::paddock::runtime::{process, types};
use crate::delivery;
use crate::instance::Message;
use crate::message::{Envelope, ExitReason, Info};
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
        mailbox: mpsc::WeakSender<Message>,
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

impl types::Host for InstanceState {}

impl process::Host for InstanceState {
    async fn self_(&mut self) -> Result<types::Pid> {
        Ok(self.pid.into())
    }

    async fn cast(&mut self, to: types::Pid, msg: types::Envelope) -> Result<()> {
        let to = Pid::from(to);
        // At most once: a cast to a dead pid, or one the target refuses, is
        // dropped without telling the sender.
        if let Err(err) = delivery::cast(&self.ctx.rt, Some(self.pid), to, msg.into()) {
            tracing::debug!(%to, error = %err, "cast dropped");
        }
        Ok(())
    }

    async fn call(
        &mut self,
        to: types::Pid,
        msg: types::Envelope,
        timeout_ms: u32,
    ) -> Result<std::result::Result<types::Envelope, types::CallError>> {
        let to = Pid::from(to);
        if to == self.pid {
            // The call would wait on this instance's own mailbox, which it
            // cannot read until this callback returns.
            return Err(Error::msg("call to self would deadlock"));
        }
        let started = Instant::now();
        let result = delivery::call(
            &self.ctx.rt,
            Some(self.pid),
            to,
            msg.into(),
            Duration::from_millis(timeout_ms.into()),
        )
        .await;
        self.clock.add_host_time(started.elapsed());
        Ok(result.map(Into::into).map_err(|e| (&e).into()))
    }

    async fn reply(&mut self, to: types::CallRef, msg: types::Envelope) -> Result<()> {
        // An unknown reference is a duplicate reply, or the caller gave up.
        if let Some(waiting) = self.pending.remove(&to) {
            let _ = waiting.send(msg.into());
        }
        Ok(())
    }

    async fn link(&mut self, other: types::Pid) -> Result<()> {
        let other = Pid::from(other);
        if other == self.pid {
            return Ok(());
        }
        let max = limit(self.host_objects.links);
        if !self.ctx.rt.links.may_link(self.pid, other, max) {
            return Err(Error::msg(LINK_LIMIT));
        }
        if let Err(reason) = delivery::link(&self.ctx.rt, self.pid, other) {
            // Linking to a dead or unreachable pid is an immediate exit
            // signal (`noproc` or `noconnection`).
            self.ctx.me.exit_signal(other, reason, false);
        }
        Ok(())
    }

    async fn unlink(&mut self, other: types::Pid) -> Result<()> {
        delivery::unlink(&self.ctx.rt, self.pid, other.into());
        Ok(())
    }

    async fn trap_exits(&mut self, on: bool) -> Result<()> {
        self.ctx.me.set_trap_exits(on);
        Ok(())
    }

    async fn monitor(&mut self, other: types::Pid) -> Result<u64> {
        let other = Pid::from(other);
        if self.ctx.rt.links.monitors_held(self.pid) >= limit(self.host_objects.monitors) {
            return Err(Error::msg(MONITOR_LIMIT));
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

    async fn demonitor(&mut self, monitor_ref: u64) -> Result<()> {
        delivery::demonitor(&self.ctx.rt, self.pid, monitor_ref);
        Ok(())
    }

    async fn register(
        &mut self,
        name: String,
    ) -> Result<std::result::Result<(), types::RegisterError>> {
        if !self.registered_names.contains(&name) {
            return Ok(Err(types::RegisterError::AccessDenied));
        }
        Ok(self
            .ctx
            .rt
            .registry
            .register(&name, self.pid)
            .map_err(|holder| types::RegisterError::Taken(holder.into())))
    }

    async fn whereis(&mut self, name: String) -> Result<Option<types::Pid>> {
        Ok(delivery::whereis(&self.ctx.rt, &name).map(Into::into))
    }

    async fn send_after(&mut self, ms: u32, msg: types::Envelope) -> Result<u64> {
        if !self.timers.has_room(limit(self.host_objects.timers)) {
            return Err(Error::msg(TIMER_LIMIT));
        }
        Ok(self.timers.start(
            self.pid,
            Duration::from_millis(ms.into()),
            msg.into(),
            self.ctx.mailbox.clone(),
            self.ctx.dropped.clone(),
        ))
    }

    async fn cancel_timer(&mut self, timer_ref: u64) -> Result<()> {
        self.timers.cancel(timer_ref);
        Ok(())
    }

    async fn exit(&mut self, other: types::Pid, reason: types::ExitReason) -> Result<()> {
        let other = Pid::from(other);
        let reason = ExitReason::from(reason);
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
            delivery::exit_signal(&self.ctx.rt, self.pid, other, reason, untrappable);
        }
        Ok(())
    }
}
