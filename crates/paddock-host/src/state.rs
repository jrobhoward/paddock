//! Per-instance store data: WASI context, resource limits and deadline
//! accounting. The `paddock:runtime/process` host functions are in
//! `process.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use paddock_bundle::manifest::Capability;
use tokio::sync::{mpsc, oneshot, watch};
use wasmtime::component::ResourceTable;
use wasmtime::{Error, ResourceLimiter, Result};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};

use crate::config::HostObjects;
use crate::failover::RunUntil;
use crate::instance::{InstanceHandle, Message};
use crate::message::Envelope;
use crate::output::{CapturedOutput, Tags};
use crate::pid::Pid;
use crate::process::{Timers, limit};
use crate::ready::Waits;
use crate::runtime::RuntimeInner;
use crate::services::config::Config;
use crate::services::egress::Egress;
use crate::services::groups::Groups;
use crate::services::keyvalue::KeyValue;
use crate::services::logging::{LogRate, Logging};
use crate::services::metrics::MetricsService;
use crate::services::secrets::Secrets;
use crate::services::sockets::{self, Sockets};
use crate::services::sql::Sql;
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
    pub(crate) ctx: ProcessContext,
    pub(crate) registered_names: Vec<String>,
    /// The caller of the call being handled, by call reference: set before
    /// `handle-call` runs, so `process.reply` works during the callback, and
    /// moved to `pending` if it returns `noreply`. Most calls never touch the
    /// map (ROADMAP B12).
    current: Option<(u64, oneshot::Sender<Envelope>)>,
    /// Callers waiting on a reply after a `noreply`, by call reference.
    pub(crate) pending: HashMap<u64, oneshot::Sender<Envelope>>,
    pub(crate) timers: Timers,
    /// Most host objects this instance may hold.
    pub(crate) host_objects: HostObjects,
    /// Whether to announce the node's failover role.
    announce_role: bool,
    /// The node's failover lease, for an instance bound to it
    /// ([`InstanceSpec::bound_to_lease`]).
    pub(crate) lease: Option<watch::Receiver<RunUntil>>,
    /// The lease ended while guest code ran, and the epoch callback
    /// interrupted it.
    pub(crate) lease_ended: bool,
    /// Pollables the guest asked to hear about (`paddock:io/ready`).
    pub(crate) ready: Waits,
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
        // No environment; stdout and stderr become logs; sockets as the
        // app's policy allows.
        let mut wasi = WasiCtx::builder();
        sockets::configure(&mut wasi, spec, ctx.rt.metrics.stats(&spec.app));
        let rate = LogRate::new(
            ctx.rt.services.logging.clone(),
            spec.services.logging.rate_limit,
        );
        wasi.stdout(CapturedOutput::new(tags.clone(), "stdout", rate.clone()))
            .stderr(CapturedOutput::new(tags.clone(), "stderr", rate));
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
        let host_objects = ctx.rt.host_objects().grant(&spec.host_objects)?;
        let lease = spec.bound_to_lease.then(|| ctx.rt.failover.run_until());
        let services = States::new(
            &ctx.rt.services,
            spec,
            &ctx.rt.extensions,
            &ctx.me,
            host_objects,
            &ctx.rt.metrics,
        )?;
        Ok(Self {
            pid,
            tags,
            wasi: wasi.build(),
            table: ResourceTable::new(),
            limiter: Limiter::new(spec.memory_max, spec.table_max, ctx.me.memory_counter()),
            clock: CallbackClock::default(),
            ctx,
            registered_names: spec.registered_names.clone(),
            current: None,
            pending: HashMap::new(),
            timers: Timers::default(),
            host_objects,
            announce_role: spec.announce_role,
            lease,
            lease_ended: false,
            ready: Waits::default(),
            services,
        })
    }

    /// Whether guest code may run now: always, unless the instance is bound
    /// to a lease that is not current.
    pub(crate) fn within_lease(&self) -> bool {
        self.lease
            .as_ref()
            .is_none_or(|l| l.borrow().allows(tokio::time::Instant::now()))
    }

    /// The context for an extension whose state is `S`, if one made it.
    pub(crate) fn extension_ctx<S: 'static>(&mut self) -> Option<ServiceCtx<'_, S>> {
        let state = self.services.extensions.get_mut::<S>()?;
        Some(ServiceCtx::new(state, &self.tags, &mut self.table))
    }

    /// Resources handed to the guest.
    pub(crate) fn table_mut(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    /// The pollables the guest waits on and the table they live in.
    pub(crate) fn ready_parts(&mut self) -> (&mut Waits, &mut ResourceTable) {
        (&mut self.ready, &mut self.table)
    }

    /// The host events the services want this instance to see now.
    pub(crate) fn announcements(&self) -> Vec<(String, Envelope)> {
        let mut events = self.services.announce(&self.tags);
        if self.announce_role
            && let Some(role) = self.ctx.rt.failover_role()
        {
            events.push((
                "failover-role".to_string(),
                Envelope::new("text/plain", role),
            ));
        }
        events
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
        to.current = self.current.take();
        to.pending = std::mem::take(&mut self.pending);
        to.timers = std::mem::take(&mut self.timers);
    }

    /// Whether the guest can be dropped and rebuilt from a snapshot without
    /// anyone noticing: no caller waits on a deferred reply, and the guest
    /// holds no host resource (a bucket, a watch, a stream), which a new
    /// guest could not name.
    pub(crate) fn passivable(&mut self) -> bool {
        self.pending.retain(|_, waiting| !waiting.is_closed());
        self.current.is_none() && self.pending.is_empty() && self.table.is_empty()
    }

    /// Remember the caller of the call about to be handled until its reply
    /// is sent.
    pub(crate) fn await_reply(&mut self, call_ref: u64, reply: oneshot::Sender<Envelope>) {
        self.current = Some((call_ref, reply));
    }

    /// The caller waiting on `call_ref`'s reply, if it has not been answered
    /// yet.
    pub(crate) fn take_reply(&mut self, call_ref: u64) -> Option<oneshot::Sender<Envelope>> {
        match self.current.take() {
            Some((r, waiting)) if r == call_ref => Some(waiting),
            other => {
                self.current = other;
                self.pending.remove(&call_ref)
            }
        }
    }

    /// `handle-call` returned `noreply`: its caller, if not answered yet,
    /// waits in `pending`. Callers that gave up (timed out) are pruned so an
    /// app that never answers does not leak them.
    pub(crate) fn defer_reply(&mut self) {
        if let Some((call_ref, waiting)) = self.current.take() {
            self.pending.retain(|_, waiting| !waiting.is_closed());
            self.pending.insert(call_ref, waiting);
        }
    }

    /// After `handle-call` returned `noreply`: an error if the instance now
    /// holds more unanswered calls than `pending_calls_max`.
    pub(crate) fn check_pending(&self) -> Result<()> {
        if self.pending.len() > limit(self.host_objects.pending_calls) {
            return Err(Error::msg(PENDING_CALL_LIMIT));
        }
        Ok(())
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
                ServiceCtx::new(&mut self.services.$field, &self.tags, &mut self.table)
            }
        }
    };
}

host_state!(Config, config);
host_state!(Egress, egress);
host_state!(Groups, groups);
host_state!(KeyValue, keyvalue);
host_state!(Logging, logging);
host_state!(Secrets, secrets);
host_state!(MetricsService, metrics);
host_state!(Sql, sql);
host_state!(Sockets, sockets);

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
/// The time the instance's task spends polling the callback
/// ([`PollTime::poll`], from `killable`): guest code, and host functions until they
/// wait. Waiting does not count, so an instance waiting on another, on a
/// timer, on I/O or for a worker after an epoch yield is not runaway code
/// (spike S1). Timing polls rather than each host call leaves wasmtime's
/// call hook off, which cost about 5% of an empty call (ROADMAP B12).
#[derive(Debug)]
pub(crate) struct CallbackClock {
    polls: Arc<PollTime>,
    limit: Duration,
    pub(crate) timed_out: bool,
}

/// Shared between the Store's data, read by the epoch callback during a
/// poll, and the task that polls.
#[derive(Debug)]
pub(crate) struct PollTime {
    /// Origin of `since`.
    base: Instant,
    /// Guest time of the callback's finished polls, in nanoseconds.
    done: AtomicU64,
    /// When the poll running now started, nanoseconds after `base`, or
    /// `NOT_POLLING`.
    since: AtomicU64,
}

const NOT_POLLING: u64 = u64::MAX;

impl PollTime {
    fn now(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(NOT_POLLING - 1)
    }

    /// Poll `fut`, counting the time as guest time.
    pub(crate) fn poll<F: Future>(
        &self,
        fut: std::pin::Pin<&mut F>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<F::Output> {
        let started = self.now();
        self.since.store(started, Ordering::Relaxed);
        let out = fut.poll(cx);
        let spent = self.now().saturating_sub(started);
        self.since.store(NOT_POLLING, Ordering::Relaxed);
        self.done.fetch_add(spent, Ordering::Relaxed);
        out
    }

    fn total(&self) -> Duration {
        let mut ns = self.done.load(Ordering::Relaxed);
        let since = self.since.load(Ordering::Relaxed);
        if since != NOT_POLLING {
            ns += self.now().saturating_sub(since);
        }
        Duration::from_nanos(ns)
    }
}

impl Default for CallbackClock {
    fn default() -> Self {
        Self {
            polls: Arc::new(PollTime {
                base: Instant::now(),
                done: AtomicU64::new(0),
                since: AtomicU64::new(NOT_POLLING),
            }),
            limit: Duration::MAX,
            timed_out: false,
        }
    }
}

impl CallbackClock {
    /// Start timing a callback allowed `limit` of guest execution; its
    /// future is polled through [`PollTime::poll`] of what this returns.
    pub(crate) fn start(&mut self, limit: Duration) -> Arc<PollTime> {
        self.polls.done.store(0, Ordering::Relaxed);
        self.limit = limit;
        self.polls.clone()
    }

    /// Whether the callback has used up its guest execution time; records a
    /// timeout if so.
    pub(crate) fn expired(&mut self) -> bool {
        if self.polls.total() > self.limit {
            self.timed_out = true;
        }
        self.timed_out
    }

    /// Guest execution time of the current callback so far.
    pub(crate) fn guest_time(&self) -> Duration {
        self.polls.total()
    }
}

/// Exit reason (`crashed`) for an instance over its pending-call limit.
const PENDING_CALL_LIMIT: &str = "pending_call_limit";
