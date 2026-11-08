//! Per-instance state on the node. The process functions are in
//! `process.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tokio::sync::{oneshot, watch};

use crate::config::HostObjects;
use crate::failover::RunUntil;
use crate::instance::InstanceHandle;
use crate::mailbox;
use crate::message::Envelope;
use crate::metrics::AppStats;
use crate::output::Tags;
use crate::pid::Pid;
use crate::process::{Timers, limit};
use crate::runtime::RuntimeInner;
use crate::services::config::Config;
use crate::services::egress::Egress;
use crate::services::groups::Groups;
use crate::services::keyvalue::KeyValue;
use crate::services::logging::Logging;
use crate::services::metrics::MetricsService;
use crate::services::secrets::Secrets;
use crate::services::sockets::Sockets;
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
    pub(crate) mailbox: mailbox::WeakSender,
    /// Messages dropped because the mailbox was full.
    pub(crate) dropped: Arc<AtomicU64>,
    /// The app's host counters (the time waited casts wait).
    pub(crate) stats: Arc<AppStats>,
}

/// The node's state for one instance. Public only as the opaque
/// [`StoreData`](crate::services::StoreData) extensions take.
pub struct InstanceState {
    pub(crate) pid: Pid,
    tags: Arc<Tags>,
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
            ctx,
            registered_names: spec.registered_names.clone(),
            current: None,
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
        Some(ServiceCtx::new(state, &self.tags))
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
            stats: self.ctx.stats.clone(),
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
        self.current.is_none() && self.pending.is_empty()
    }

    /// Remember the caller of the call about to be handled until its reply
    /// is sent.
    pub(crate) fn await_reply(&mut self, call_ref: u64, reply: oneshot::Sender<Envelope>) {
        self.current = Some((call_ref, reply));
    }

    /// The caller waiting on `call_ref`'s reply, if it has not been answered
    /// yet. Taking it marks the instance as having woken another task, since
    /// the caller is woken by the reply.
    pub(crate) fn take_reply(&mut self, call_ref: u64) -> Option<oneshot::Sender<Envelope>> {
        let waiting = match self.current.take() {
            Some((r, waiting)) if r == call_ref => Some(waiting),
            other => {
                self.current = other;
                self.pending.remove(&call_ref)
            }
        };
        if waiting.is_some() {
            self.ctx.me.woke_others();
        }
        waiting
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
    pub(crate) fn check_pending(&self) -> Result<(), String> {
        if self.pending.len() > limit(self.host_objects.pending_calls) {
            return Err(PENDING_CALL_LIMIT.to_string());
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
                ServiceCtx::new(&mut self.services.$field, &self.tags)
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

/// Exit reason (`crashed`) for an instance over its pending-call limit.
const PENDING_CALL_LIMIT: &str = "pending_call_limit";
