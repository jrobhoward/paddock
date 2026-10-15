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
use crate::runtime::RuntimeInner;
use crate::services::config::Config;
use crate::services::groups::Groups;
use crate::services::keyvalue::KeyValue;
use crate::services::logging::{LogRate, Logging};
use crate::services::metrics::MetricsService;
use crate::services::secrets::Secrets;
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
    /// Callers waiting on a reply, by call reference. An entry is added
    /// before `handle-call` runs, so `process.reply` works during the
    /// callback as well as after a `noreply`.
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
            pending: HashMap::new(),
            timers: Timers::default(),
            host_objects,
            announce_role: spec.announce_role,
            lease,
            lease_ended: false,
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
        to.pending = std::mem::take(&mut self.pending);
        to.timers = std::mem::take(&mut self.timers);
    }

    /// Whether the guest can be dropped and rebuilt from a snapshot without
    /// anyone noticing: no caller waits on a deferred reply, and the guest
    /// holds no host resource (a bucket, a watch, a stream), which a new
    /// guest could not name.
    pub(crate) fn passivable(&mut self) -> bool {
        self.pending.retain(|_, waiting| !waiting.is_closed());
        self.pending.is_empty() && self.table.is_empty()
    }

    /// Remember a caller until its reply is sent. Callers that gave up (timed
    /// out) are pruned so an app that never answers does not leak them.
    pub(crate) fn await_reply(&mut self, call_ref: u64, reply: oneshot::Sender<Envelope>) {
        self.pending.retain(|_, waiting| !waiting.is_closed());
        self.pending.insert(call_ref, reply);
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
host_state!(Groups, groups);
host_state!(KeyValue, keyvalue);
host_state!(Logging, logging);
host_state!(Secrets, secrets);
host_state!(MetricsService, metrics);

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

/// Exit reason (`crashed`) for an instance over its pending-call limit.
const PENDING_CALL_LIMIT: &str = "pending_call_limit";
