//! Per-instance store data: WASI context, resource limits, deadline
//! accounting and the `paddock:runtime` host functions.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use paddock_bundle::manifest::Capability;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use wasmtime::component::ResourceTable;
use wasmtime::{Error, ResourceLimiter, Result};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};

use crate::bindings::paddock::runtime::{process, types};
use crate::instance::{InstanceHandle, Message};
use crate::message::{Envelope, ExitReason, Info};
use crate::output::{CapturedOutput, Tags};
use crate::pid::Pid;
use crate::probe;
use crate::runtime::RuntimeInner;
use crate::services::config::Config;
use crate::services::keyvalue::KeyValue;
use crate::services::logging::Logging;
use crate::services::{HostState, Service, ServiceCtx, States};
use crate::spec::InstanceSpec;

/// What an instance's host functions reach outside its own Store.
pub(crate) struct ProcessContext {
    pub(crate) rt: Arc<RuntimeInner>,
    /// This instance's own handle, for its trap-exits flag and signals.
    pub(crate) me: InstanceHandle,
    /// The instance's own mailbox, for timers. Weak, so it never keeps the
    /// mailbox open.
    pub(crate) mailbox: mpsc::WeakSender<Message>,
    /// Messages dropped because the mailbox was full.
    pub(crate) dropped: Arc<AtomicU64>,
}

/// Store data for one instance. Public only as the opaque
/// [`StoreData`](crate::services::StoreData) extensions link against.
pub struct InstanceState {
    pub(crate) pid: Pid,
    tags: Arc<Tags>,
    wasi: WasiCtx,
    table: ResourceTable,
    pub(crate) limiter: Limiter,
    pub(crate) clock: CallbackClock,
    ctx: ProcessContext,
    registered_names: Vec<String>,
    /// Callers waiting on a reply, by call reference. An entry is added
    /// before `handle-call` runs, so `process.reply` works during the
    /// callback as well as after a `noreply`.
    pub(crate) pending: HashMap<u64, oneshot::Sender<Envelope>>,
    pub(crate) timers: Timers,
    services: States,
}

impl InstanceState {
    /// # Errors
    ///
    /// A message if a preopened directory cannot be opened.
    pub(crate) fn new(pid: Pid, spec: &InstanceSpec, ctx: ProcessContext) -> Result<Self, String> {
        let tags = Arc::new(Tags {
            app: spec.app.clone(),
            version: spec.version.clone(),
            pid,
        });
        // No environment and no sockets; stdout and stderr become logs.
        let mut wasi = WasiCtx::builder();
        wasi.stdout(CapturedOutput::new(tags.clone(), "stdout"))
            .stderr(CapturedOutput::new(tags.clone(), "stderr"));
        if spec.granted.contains(&Capability::Filesystem) {
            for p in &spec.preopens {
                let perms = if p.writable {
                    FsPerms::ReadWrite
                } else {
                    FsPerms::ReadOnly
                };
                wasi.preopened_dir(&p.host, &p.guest, perms)
                    .map_err(|e| format!("preopen {} -> {}: {e}", p.host.display(), p.guest))?;
            }
        }
        let services = States::new(&ctx.rt.services, spec, &ctx.rt.extensions)?;
        Ok(Self {
            pid,
            tags,
            wasi: wasi.build(),
            table: ResourceTable::new(),
            limiter: Limiter::new(spec.memory_max, spec.table_max, ctx.me.memory_counter()),
            clock: CallbackClock::default(),
            ctx,
            registered_names: spec.registered_names.clone(),
            pending: HashMap::new(),
            timers: Timers::default(),
            services,
        })
    }

    /// The context for an extension whose state is `S`, if one made it.
    pub(crate) fn extension_ctx<S: 'static>(&mut self) -> Option<ServiceCtx<'_, S>> {
        let state = self.services.extensions.get_mut::<S>()?;
        Some(ServiceCtx::new(
            state,
            &self.tags,
            &mut self.table,
            &mut self.clock,
        ))
    }

    /// Resources handed to the guest.
    pub(crate) fn table_mut(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    /// The host events the services want this instance to see now.
    pub(crate) fn announcements(&self) -> Vec<(String, Envelope)> {
        self.services.announce(&self.tags)
    }

    /// A context for a new Store of the same instance (an upgrade).
    pub(crate) fn context(&self) -> ProcessContext {
        ProcessContext {
            rt: self.ctx.rt.clone(),
            me: self.ctx.me.clone(),
            mailbox: self.ctx.mailbox.clone(),
            dropped: self.ctx.dropped.clone(),
        }
    }

    /// Move what belongs to the instance rather than to one Store (callers
    /// awaiting deferred replies, pending timers) into `to`.
    pub(crate) fn hand_over(&mut self, to: &mut Self) {
        to.pending = std::mem::take(&mut self.pending);
        to.timers = std::mem::take(&mut self.timers);
    }

    /// Remember a caller until its reply is sent. Callers that gave up (timed
    /// out) are pruned so an app that never answers does not leak them.
    pub(crate) fn await_reply(&mut self, call_ref: u64, reply: oneshot::Sender<Envelope>) {
        self.pending.retain(|_, waiting| !waiting.is_closed());
        self.pending.insert(call_ref, reply);
    }
}

impl std::fmt::Debug for InstanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceState")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// Each service reaches its own state in `services`, plus the shared
/// facilities.
macro_rules! host_state {
    ($service:ty, $field:ident) => {
        impl HostState<$service> for InstanceState {
            fn service(&mut self) -> ServiceCtx<'_, <$service as Service>::Instance> {
                ServiceCtx::new(
                    &mut self.services.$field,
                    &self.tags,
                    &mut self.table,
                    &mut self.clock,
                )
            }
        }
    };
}

host_state!(Config, config);
host_state!(KeyValue, keyvalue);
host_state!(Logging, logging);

impl WasiView for InstanceState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Enforces `limits.memory_max` and `limits.table_max`, and remembers which
/// one was hit so the exit reason can say so.
pub(crate) struct Limiter {
    memory_max: usize,
    table_max: Option<usize>,
    pub(crate) memory_exceeded: bool,
    pub(crate) table_exceeded: bool,
    /// Linear memory size as last allowed to grow, for `ps`.
    memory: Arc<AtomicU64>,
}

impl Limiter {
    fn new(memory_max: u64, table_max: Option<u32>, memory: Arc<AtomicU64>) -> Self {
        Self {
            memory_max: usize::try_from(memory_max).unwrap_or(usize::MAX),
            table_max: table_max.map(|t| usize::try_from(t).unwrap_or(usize::MAX)),
            memory_exceeded: false,
            table_exceeded: false,
            memory,
        }
    }
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        if desired > self.memory_max {
            self.memory_exceeded = true;
            return Err(Error::msg("memory_limit"));
        }
        self.memory.store(
            u64::try_from(desired).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        if self.table_max.is_some_and(|max| desired > max) {
            self.table_exceeded = true;
            return Err(Error::msg("table_limit"));
        }
        Ok(true)
    }
}

/// Guest execution time of the current callback.
///
/// The deadline counts guest execution only: host imports add the time they
/// spend suspended to `host_time`, which is subtracted, so an instance waiting
/// on another is not runaway code (spike S1).
#[derive(Debug)]
pub(crate) struct CallbackClock {
    started: Instant,
    host_time: Duration,
    limit: Duration,
    pub(crate) timed_out: bool,
}

impl Default for CallbackClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            host_time: Duration::ZERO,
            limit: Duration::MAX,
            timed_out: false,
        }
    }
}

impl CallbackClock {
    /// Start timing a callback allowed `limit` of guest execution.
    pub(crate) fn start(&mut self, limit: Duration) {
        self.started = Instant::now();
        self.host_time = Duration::ZERO;
        self.limit = limit;
    }

    /// Whether the callback has used up its guest execution time; records a
    /// timeout if so.
    pub(crate) fn expired(&mut self) -> bool {
        let running = self.started.elapsed().saturating_sub(self.host_time);
        if running > self.limit {
            self.timed_out = true;
        }
        self.timed_out
    }

    /// Guest execution time of the current callback so far: wall time
    /// minus time spent in host imports.
    pub(crate) fn guest_time(&self) -> Duration {
        self.started.elapsed().saturating_sub(self.host_time)
    }

    /// Exclude time spent suspended in a host import from the deadline.
    pub(crate) fn add_host_time(&mut self, spent: Duration) {
        self.host_time += spent;
    }
}

/// An instance's pending `send-after` timers. Dropping the set (when the
/// instance exits) cancels them.
#[derive(Debug, Default)]
pub(crate) struct Timers {
    next: u64,
    active: HashMap<u64, AbortHandle>,
}

impl Timers {
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
        if let Some(target) = self.ctx.rt.table.lookup(to)
            && let Err(err) = target.cast_from(Some(self.pid), msg.into())
        {
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
        let Some(target) = self.ctx.rt.table.lookup(to) else {
            return Ok(Err(types::CallError::Noproc));
        };
        let started = Instant::now();
        let result = target
            .call_from(
                Some(self.pid),
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
        let table = &self.ctx.rt.table;
        if !self
            .ctx
            .rt
            .links
            .link(self.pid, other, |p| table.lookup(p).is_some())
        {
            // Linking to a dead pid is an immediate `noproc` exit signal.
            self.ctx.me.exit_signal(other, ExitReason::Noproc, false);
        }
        Ok(())
    }

    async fn unlink(&mut self, other: types::Pid) -> Result<()> {
        self.ctx.rt.links.unlink(self.pid, other.into());
        Ok(())
    }

    async fn trap_exits(&mut self, on: bool) -> Result<()> {
        self.ctx.me.set_trap_exits(on);
        Ok(())
    }

    async fn monitor(&mut self, other: types::Pid) -> Result<u64> {
        let other = Pid::from(other);
        let table = &self.ctx.rt.table;
        let (reference, alive) = self
            .ctx
            .rt
            .links
            .monitor(self.pid, other, |p| table.lookup(p).is_some());
        if !alive {
            self.ctx.me.signal(Info::Down {
                reference,
                pid: other,
                reason: ExitReason::Noproc,
            });
        }
        Ok(reference)
    }

    async fn demonitor(&mut self, monitor_ref: u64) -> Result<()> {
        self.ctx.rt.links.demonitor(self.pid, monitor_ref);
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
        let rt = &self.ctx.rt;
        let found = rt
            .registry
            .whereis(&name)
            .or_else(|| rt.pool_member(&name).map(|h| h.pid()));
        Ok(found.map(Into::into))
    }

    async fn send_after(&mut self, ms: u32, msg: types::Envelope) -> Result<u64> {
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
        } else if let Some(target) = self.ctx.rt.table.lookup(other) {
            target.exit_signal(self.pid, reason, untrappable);
        }
        Ok(())
    }
}
