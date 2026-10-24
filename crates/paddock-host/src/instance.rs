//! One instance: a Store, its limits, a bounded mailbox and the task that
//! runs the callback loop.
//!
//! The task instantiates the component and runs `init`, then handles one
//! message at a time until it is stopped or a callback fails, then runs
//! `terminate` if the stop is graceful. Every failure becomes an
//! [`ExitReason`]; nothing in a guest can panic the host.
//!
//! Killing an instance drops the in-flight call future and discards the
//! Store. A Store whose call was dropped is poisoned, so it is never reused
//! (spike S1).
//!
//! An instance with [`InstanceSpec::passivate_after`] is passivated after
//! that long with nothing to do (spike S11): `save-snapshot`, then the Store
//! is dropped and its data kept (timers, waiting callers, services; the pid,
//! mailbox, links, monitors and names live outside it). It holds no Store
//! slot then. The next message, signal or operation takes a slot, builds a
//! new Store on the kept data and runs `load-snapshot` with the instance's
//! own version; a `snapshot` operation is answered from the kept bytes
//! without waking it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::Poll;
use std::time::Duration;

use atomic_waker::AtomicWaker;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tracing::Instrument;
use wasmtime::{Store, Trap, UpdateDeadline};

use crate::acl::Acl;
use crate::bindings::{CallResult, PaddockApp};
use crate::delivery;
use crate::error::describe;
use crate::failover::RunUntil;
use crate::health;
use crate::lifecycle::LifecycleState;
use crate::message::{CallError, Envelope, ExitReason, Info};
use crate::metrics::AppStats;
use crate::pid::Pid;
use crate::probe;
use crate::runtime::{LoadedApp, RuntimeInner};
use crate::spec::InstanceSpec;
use crate::state::{InstanceState, PollTime, ProcessContext};
use crate::upgrade::UpgradeError;

/// Something waiting in a mailbox.
#[derive(Debug)]
pub(crate) enum Message {
    Call {
        msg: Envelope,
        reply: oneshot::Sender<Envelope>,
    },
    Cast(Envelope),
    Info(Info),
}

/// A host operation on an instance, handled between callbacks ahead of the
/// mailbox.
enum Op {
    Upgrade {
        app: LoadedApp,
        spec: Box<InstanceSpec>,
        from_version: String,
        reply: oneshot::Sender<Result<Duration, UpgradeError>>,
    },
    Snapshot {
        reply: oneshot::Sender<Result<Vec<u8>, SnapshotError>>,
    },
    /// Take the state to start the instance on another node: a snapshot,
    /// after which the instance handles nothing more and exits `normal`
    /// when it is stopped, without `terminate`.
    MoveOut {
        reply: oneshot::Sender<Result<Vec<u8>, SnapshotError>>,
    },
    /// Send the services' current host events again.
    Announce,
}

/// What woke an instance waiting for its next message: the message (or
/// what to do instead), or a host operation to run first.
enum Woke {
    Next(Next),
    Op(Box<Op>),
}

/// Why reading an instance's state failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SnapshotError {
    /// The instance had exited.
    #[error("instance is not running")]
    NotRunning,
    /// `save-snapshot` failed. The Store cannot continue after a trap, so the
    /// instance exited with this reason and its supervisor restarts it.
    #[error("save-snapshot failed: {0}")]
    Failed(ExitReason),
    /// A native-process app takes no snapshot.
    #[error("a native-process app takes no snapshot")]
    NotSupported,
}

/// How often an instance waiting for a Store slot asks an idle one to
/// passivate.
const SLOT_ASK_AGAIN: Duration = Duration::from_millis(10);

/// Why no Store slot was taken.
enum NoSlot {
    /// The instance was killed while it waited, with this reason.
    Killed(ExitReason),
    /// None freed within the wait (its `init_timeout`).
    Timeout,
}

impl NoSlot {
    /// As an exit reason: a timeout is `crashed: no_slot`.
    fn reason(self) -> ExitReason {
        match self {
            Self::Killed(reason) => reason,
            Self::Timeout => ExitReason::Crashed("no_slot".to_string()),
        }
    }
}

/// How [`InstanceTask::serve`] ended without a failure.
enum Served {
    /// A stop was requested, with this reason.
    Stop(ExitReason),
    /// Nothing to do for `passivate_after`.
    Idle,
}

/// What [`InstanceTask::receive`] found.
enum Next {
    Message(Message),
    /// Nothing to handle yet: check for a stop and receive again.
    Again,
    Done(Result<Served, ExitReason>),
}

/// A stop request. A kill overrides a graceful shutdown; nothing overrides a
/// kill. A kill carries the exit reason: `killed` for an operator kill, or
/// the reason of an exit signal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Control {
    Run,
    Shutdown(ExitReason),
    Kill(ExitReason),
}

/// A handle to one instance (one incarnation). Cheap to clone.
#[derive(Debug, Clone)]
pub struct InstanceHandle {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    pid: Pid,
    app: String,
    version: std::sync::Mutex<String>,
    mailbox: mpsc::Sender<Message>,
    control: watch::Sender<Control>,
    kill: Arc<KillSignal>,
    state: watch::Receiver<LifecycleState>,
    exit: watch::Receiver<Option<ExitReason>>,
    message_max_bytes: AtomicU64,
    /// Which apps it may send to and accept from; replaced at an upgrade.
    acl: std::sync::RwLock<Arc<Acl>>,
    dropped: Arc<AtomicU64>,
    /// Boxed: rare, and a channel allocates its first block of 32 slots
    /// up front, which unboxed would cost 6 KiB per instance.
    ops: mpsc::UnboundedSender<Box<Op>>,
    /// Exit and down notifications. Unbounded: OTP never drops signals, and
    /// their number is bounded by links and monitors.
    signals: mpsc::UnboundedSender<Info>,
    trap_exits: AtomicBool,
    /// Bytes of linear memory, as the Store's limiter last allowed it to
    /// grow.
    memory: Arc<AtomicU64>,
    /// Why the previous incarnation exited, when a supervisor restarted it.
    last_exit: std::sync::Mutex<Option<ExitReason>>,
}

/// Something that happened to an instance, from
/// [`crate::Runtime::instance_events`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstanceEvent {
    /// The instance exited, for whatever reason (a stop is `normal` or
    /// `shutdown`).
    Exited {
        /// Its pid.
        pid: Pid,
        /// The app it ran.
        app: String,
        /// Why.
        reason: ExitReason,
    },
    /// A supervisor started `pid` in place of `previous`, which exited.
    Restarted {
        /// The new incarnation.
        pid: Pid,
        /// The one it replaces.
        previous: Pid,
        /// The supervisor's child id.
        child: String,
    },
}

impl InstanceHandle {
    /// The instance's pid.
    #[must_use]
    pub fn pid(&self) -> Pid {
        self.shared.pid
    }

    /// The app this instance runs.
    #[must_use]
    pub fn app(&self) -> &str {
        &self.shared.app
    }

    /// Which apps it may send to and accept from, as its current version's
    /// manifest says.
    pub(crate) fn acl(&self) -> Arc<Acl> {
        self.shared
            .acl
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The version it runs (changes on upgrade).
    #[must_use]
    pub fn version(&self) -> String {
        self.shared
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Messages waiting in the mailbox.
    #[must_use]
    pub fn queued(&self) -> usize {
        let m = &self.shared.mailbox;
        m.max_capacity().saturating_sub(m.capacity())
    }

    /// The current lifecycle state.
    #[must_use]
    pub fn state(&self) -> LifecycleState {
        *self.shared.state.borrow()
    }

    /// A receiver that sees every lifecycle change.
    #[must_use]
    pub fn subscribe_state(&self) -> watch::Receiver<LifecycleState> {
        self.shared.state.clone()
    }

    /// Why the instance exited, or `None` while it runs.
    #[must_use]
    pub fn exit_reason(&self) -> Option<ExitReason> {
        self.shared.exit.borrow().clone()
    }

    /// Messages dropped because the mailbox was full: casts and timer
    /// messages. Calls are refused with `overloaded` instead.
    #[must_use]
    pub fn dropped_messages(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Bytes of linear memory the instance has grown to (its largest memory
    /// as last grown; 0 before it starts).
    #[must_use]
    pub fn memory_bytes(&self) -> u64 {
        self.shared.memory.load(Ordering::Relaxed)
    }

    /// How many times this instance has been restarted: its incarnation
    /// minus one.
    #[must_use]
    pub fn restarts(&self) -> u32 {
        self.shared.pid.incarnation.saturating_sub(1)
    }

    /// Why the previous incarnation exited, if a supervisor restarted this
    /// one after an exit it saw.
    #[must_use]
    pub fn last_exit(&self) -> Option<ExitReason> {
        self.shared
            .last_exit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_last_exit(&self, reason: ExitReason) {
        *self
            .shared
            .last_exit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
    }

    /// The counter the Store's limiter records memory growth in.
    pub(crate) fn memory_counter(&self) -> Arc<AtomicU64> {
        self.shared.memory.clone()
    }

    /// Wait for the instance to exit and return why.
    pub async fn wait(&self) -> ExitReason {
        let mut exit = self.shared.exit.clone();
        match exit.wait_for(Option::is_some).await {
            Ok(reason) => reason.clone().unwrap_or(ExitReason::Killed),
            // The task ended without publishing a reason: it was aborted.
            Err(_) => ExitReason::Killed,
        }
    }

    /// Queue a cast. Content type and schema id reach the guest untouched.
    ///
    /// # Errors
    ///
    /// [`CallError::TooLarge`] if the body exceeds the instance's
    /// `message_max_bytes`, [`CallError::Overloaded`] if the mailbox is full
    /// (the cast is dropped and counted), [`CallError::Noproc`] if the
    /// instance has exited.
    pub fn cast(&self, msg: Envelope) -> Result<(), CallError> {
        self.cast_from(None, msg)
    }

    /// [`InstanceHandle::cast`] from instance `from`, for the probes.
    pub(crate) fn cast_from(&self, from: Option<Pid>, msg: Envelope) -> Result<(), CallError> {
        let bytes = msg.body.len();
        let result = self.check_size(&msg).and_then(|()| {
            self.shared
                .mailbox
                .try_send(Message::Cast(msg))
                .map_err(|e| match e {
                    mpsc::error::TrySendError::Full(_) => {
                        self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                        probe::mailbox_drop(self.pid(), "cast");
                        CallError::Overloaded
                    }
                    mpsc::error::TrySendError::Closed(_) => CallError::Noproc,
                })
        });
        probe::cast_sent(from, self.pid(), bytes, &result);
        result
    }

    fn check_size(&self, msg: &Envelope) -> Result<(), CallError> {
        let len = u64::try_from(msg.body.len()).unwrap_or(u64::MAX);
        if len > self.shared.message_max_bytes.load(Ordering::Relaxed) {
            return Err(CallError::TooLarge);
        }
        Ok(())
    }

    /// Send a call and wait up to `timeout` for the reply.
    ///
    /// # Errors
    ///
    /// [`CallError::TooLarge`] at once if the body exceeds the instance's
    /// `message_max_bytes`, [`CallError::Overloaded`] at once if the mailbox
    /// is full,
    /// [`CallError::Timeout`] if no reply arrives in time (a late reply is
    /// dropped), and [`CallError::Exited`] if the instance exits first.
    pub async fn call(&self, msg: Envelope, timeout: Duration) -> Result<Envelope, CallError> {
        self.call_from(None, msg, timeout).await
    }

    /// [`InstanceHandle::call`] from instance `from`, for the probes.
    pub(crate) async fn call_from(
        &self,
        from: Option<Pid>,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, CallError> {
        let to = self.pid();
        let call = probe::call_start(from, to, msg.body.len());
        let result = self.send_call(msg, timeout).await;
        probe::call_done(call, to, &result);
        result
    }

    async fn send_call(&self, msg: Envelope, timeout: Duration) -> Result<Envelope, CallError> {
        self.check_size(&msg)?;
        let (reply, answer) = oneshot::channel();
        let queued = self.shared.mailbox.try_send(Message::Call { msg, reply });
        let waiting = async {
            match queued {
                Ok(()) => match answer.await {
                    Ok(envelope) => Ok(envelope),
                    Err(_) => Err(CallError::Exited(self.wait().await)),
                },
                Err(mpsc::error::TrySendError::Full(_)) => Err(CallError::Overloaded),
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    Err(CallError::Exited(self.wait().await))
                }
            }
        };
        // Most replies arrive before the caller next runs: yield once and
        // arm the timer only if this one has not. A timer per call (the
        // wheel's insert and removal) cost about 8% of an empty call
        // (`call_split`, ROADMAP B12). The deadline is from the start, so a
        // timeout still fires on time.
        let deadline = tokio::time::Instant::now() + timeout;
        let mut waiting = std::pin::pin!(waiting);
        tokio::select! {
            biased;
            done = &mut waiting => return done,
            () = tokio::task::yield_now() => {}
        }
        tokio::time::timeout_at(deadline, waiting)
            .await
            .unwrap_or(Err(CallError::Timeout))
    }

    /// Stop gracefully: finish the current callback, run `terminate(reason)`
    /// within the shutdown timeout, then exit with `reason`. Messages still
    /// queued are dropped and their callers get `exited`.
    pub fn shutdown(&self, reason: ExitReason) {
        self.shared.control.send_if_modified(|c| {
            let run = *c == Control::Run;
            if run {
                *c = Control::Shutdown(reason);
            }
            run
        });
    }

    /// Swap this instance to another component, keeping its pid, mailbox,
    /// links, monitors, registered names, timers and callers awaiting
    /// deferred replies. Between callbacks, the old version's
    /// `save-snapshot` output is passed to the new version's
    /// `load-snapshot(from_version, ...)`; `init` does not run. Messages
    /// sent meanwhile wait in the mailbox. Returns how long the instance was
    /// paused.
    ///
    /// # Errors
    ///
    /// [`UpgradeError::Instantiate`] or [`UpgradeError::LoadFailed`] when
    /// the new version fails (the old one keeps running unchanged),
    /// [`UpgradeError::SaveFailed`] when the old version's `save-snapshot`
    /// fails (the instance then exits), [`UpgradeError::NotRunning`] if it
    /// has exited.
    pub async fn upgrade(
        &self,
        app: &LoadedApp,
        spec: InstanceSpec,
        from_version: &str,
    ) -> Result<Duration, UpgradeError> {
        let (reply, answer) = oneshot::channel();
        self.shared
            .ops
            .send(Box::new(Op::Upgrade {
                app: app.clone(),
                spec: Box::new(spec),
                from_version: from_version.to_string(),
                reply,
            }))
            .map_err(|_| UpgradeError::NotRunning)?;
        answer.await.unwrap_or(Err(UpgradeError::NotRunning))
    }

    /// The instance's state as its `save-snapshot` writes it, taken between
    /// callbacks (OTP's `sys:get_state`). `save-snapshot` must not change
    /// state, so the instance carries on. If it traps, the instance exits and
    /// its supervisor restarts it, as after a failed upgrade.
    ///
    /// # Errors
    ///
    /// [`SnapshotError::NotRunning`] if it has exited, and
    /// [`SnapshotError::Failed`] if `save-snapshot` fails.
    pub async fn snapshot(&self) -> Result<Vec<u8>, SnapshotError> {
        let (reply, answer) = oneshot::channel();
        self.shared
            .ops
            .send(Box::new(Op::Snapshot { reply }))
            .map_err(|_| SnapshotError::NotRunning)?;
        answer.await.unwrap_or(Err(SnapshotError::NotRunning))
    }

    /// The instance's state for a move to another node, taken as
    /// [`InstanceHandle::snapshot`] takes it; the instance then handles no
    /// more messages, and a stop ends it at once, without `terminate`: a
    /// graceful one with reason `normal`, so links do not carry it (the
    /// instance lives on elsewhere), a kill with the kill's reason.
    /// Messages left in its mailbox are dropped then.
    ///
    /// # Errors
    ///
    /// As for [`InstanceHandle::snapshot`]; the instance carries on only if
    /// it was not running.
    pub(crate) async fn move_out(&self) -> Result<Vec<u8>, SnapshotError> {
        let (reply, answer) = oneshot::channel();
        self.shared
            .ops
            .send(Box::new(Op::MoveOut { reply }))
            .map_err(|_| SnapshotError::NotRunning)?;
        answer.await.unwrap_or(Err(SnapshotError::NotRunning))
    }

    /// Have the instance send its services' host events again, after an
    /// operator change such as a new log level. Ignored once it has exited.
    pub(crate) fn announce(&self) {
        let _ = self.shared.ops.send(Box::new(Op::Announce));
    }

    /// Stop now with reason `killed`, without `terminate`. A callback in
    /// progress is abandoned.
    pub fn kill(&self) {
        self.stop_now(ExitReason::Killed);
    }

    /// Stop now with `reason`, without `terminate`; the first stop reason
    /// wins.
    pub(crate) fn kill_with(&self, reason: ExitReason) {
        self.stop_now(reason);
    }

    /// Stop now with `reason`, without `terminate`. The first stop reason
    /// wins.
    fn stop_now(&self, reason: ExitReason) {
        let first = self.shared.control.send_if_modified(|c| {
            let first = !matches!(c, Control::Kill(_));
            if first {
                *c = Control::Kill(reason);
            }
            first
        });
        if first {
            self.shared.kill.killed.store(true, Ordering::Release);
            self.shared.kill.waker.wake();
        }
    }

    /// Whether linked exits arrive as `info.exit` instead of stopping this
    /// instance.
    #[must_use]
    pub fn traps_exits(&self) -> bool {
        self.shared.trap_exits.load(Ordering::Relaxed)
    }

    pub(crate) fn set_trap_exits(&self, on: bool) {
        self.shared.trap_exits.store(on, Ordering::Relaxed);
    }

    /// Queue a notification for `handle-info`.
    pub(crate) fn signal(&self, info: Info) {
        // Fails only once the instance has exited, when nobody needs it.
        let _ = self.shared.signals.send(info);
    }

    /// Deliver an exit signal from `from`, with OTP's rules. An untrappable
    /// signal (`exit(pid, killed)`) always stops the instance with `killed`.
    /// Otherwise an instance that traps exits gets `info.exit`; one that does
    /// not ignores `normal` and stops with any other reason.
    pub(crate) fn exit_signal(&self, from: Pid, reason: ExitReason, untrappable: bool) {
        if untrappable {
            self.stop_now(ExitReason::Killed);
        } else if self.traps_exits() {
            self.signal(Info::Exit { from, reason });
        } else if reason != ExitReason::Normal {
            self.stop_now(reason);
        }
    }

    /// Stop this instance with `reason` without `terminate`, as an exit
    /// signal to itself does.
    pub(crate) fn exit_self(&self, reason: ExitReason) {
        self.stop_now(reason);
    }
}

/// The receiving half of an instance, moved into its task.
/// Aborts its task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct InstanceTask {
    pid: Pid,
    me: InstanceHandle,
    mailbox: mpsc::Receiver<Message>,
    signals: mpsc::UnboundedReceiver<Info>,
    ops: mpsc::UnboundedReceiver<Box<Op>>,
    self_mailbox: mpsc::WeakSender<Message>,
    dropped: Arc<AtomicU64>,
    control: watch::Receiver<Control>,
    kill: Arc<KillSignal>,
    state: watch::Sender<LifecycleState>,
    exit: watch::Sender<Option<ExitReason>>,
    /// The app's host counters.
    stats: Arc<AppStats>,
    /// What woke a passivated instance, handled first once it is awake.
    held: Option<Message>,
    held_op: Option<Box<Op>>,
    /// The next call reference, unique for the instance's whole life: a
    /// guest woken from a snapshot may still hold the reference of a call
    /// whose caller gave up.
    next_ref: u64,
}

/// A pid's handle and process context with no mailbox behind them, for a
/// connection handler: it can call, cast and link like an instance, but
/// messages, timers and exit signals sent to it are dropped.
pub(crate) fn detached(
    pid: Pid,
    spec: &InstanceSpec,
    rt: Arc<RuntimeInner>,
) -> (InstanceHandle, ProcessContext) {
    let (handle, task) = prepare(pid, spec, rt.metrics.stats(&spec.app));
    let ctx = ProcessContext {
        rt,
        me: handle.clone(),
        mailbox: task.self_mailbox.clone(),
        dropped: task.dropped.clone(),
    };
    // Dropping the task closes the mailbox and signal receivers.
    drop(task);
    (handle, ctx)
}

/// Create the channels for a new instance.
pub(crate) fn prepare(
    pid: Pid,
    spec: &InstanceSpec,
    stats: Arc<AppStats>,
) -> (InstanceHandle, InstanceTask) {
    let (mailbox_tx, mailbox_rx) = mpsc::channel(spec.mailbox_max.max(1));
    let (control_tx, control_rx) = watch::channel(Control::Run);
    let (state_tx, state_rx) = watch::channel(LifecycleState::Starting);
    let (exit_tx, exit_rx) = watch::channel(None);
    let dropped = Arc::new(AtomicU64::new(0));
    let kill = Arc::new(KillSignal::default());
    let (signals_tx, signals_rx) = mpsc::unbounded_channel();
    let (ops_tx, ops_rx) = mpsc::unbounded_channel();
    let self_mailbox = mailbox_tx.downgrade();
    let handle = InstanceHandle {
        shared: Arc::new(Shared {
            pid,
            app: spec.app.clone(),
            version: std::sync::Mutex::new(spec.version.clone()),
            mailbox: mailbox_tx,
            control: control_tx,
            kill: kill.clone(),
            state: state_rx,
            exit: exit_rx,
            message_max_bytes: AtomicU64::new(spec.message_max_bytes),
            acl: std::sync::RwLock::new(Arc::new(spec.acl.clone())),
            dropped: dropped.clone(),
            ops: ops_tx,
            signals: signals_tx,
            trap_exits: AtomicBool::new(false),
            memory: Arc::new(AtomicU64::new(0)),
            last_exit: std::sync::Mutex::new(None),
        }),
    };
    let task = InstanceTask {
        stats,
        pid,
        me: handle.clone(),
        mailbox: mailbox_rx,
        signals: signals_rx,
        ops: ops_rx,
        self_mailbox,
        dropped,
        control: control_rx,
        kill,
        state: state_tx,
        exit: exit_tx,
        held: None,
        held_op: None,
        next_ref: 1,
    };
    (handle, task)
}

impl InstanceTask {
    /// Run the instance to completion and publish its exit reason.
    pub(crate) async fn run(mut self, rt: Arc<RuntimeInner>, app: LoadedApp, spec: InstanceSpec) {
        let span = tracing::info_span!(
            "instance",
            app = %spec.app,
            version = %spec.version,
            pid = %self.pid,
            node = self.pid.node,
        );
        let reason = self
            .lifecycle(rt.clone(), &app, &spec)
            .instrument(span.clone())
            .await;

        // Names first, so once the pid is gone from the table its names are
        // free for the next incarnation.
        rt.registry.unregister_all(self.pid);
        rt.table.remove(self.pid);
        self.mailbox.close();
        while self.mailbox.try_recv().is_ok() {}
        self.signals.close();
        // Fail any upgrade still queued.
        self.ops.close();
        while let Ok(op) = self.ops.try_recv() {
            match *op {
                Op::Upgrade { reply, .. } => {
                    let _ = reply.send(Err(UpgradeError::NotRunning));
                }
                Op::Snapshot { reply } | Op::MoveOut { reply } => {
                    let _ = reply.send(Err(SnapshotError::NotRunning));
                }
                Op::Announce => {}
            }
        }
        probe::instance_exit(self.pid, &reason);
        self.stats.exited(&reason);
        self.stats
            .dropped
            .fetch_add(self.dropped.load(Ordering::Relaxed), Ordering::Relaxed);
        delivery::notify_exit(&rt, self.pid, &reason);
        let _ = rt.instance_events.send(InstanceEvent::Exited {
            pid: self.pid,
            app: spec.app.clone(),
            reason: reason.clone(),
        });
        let _enter = span.enter();
        if reason.is_normal() {
            tracing::info!(%reason, "instance stopped");
            self.set_state(LifecycleState::Stopped);
        } else {
            tracing::warn!(%reason, "instance failed");
            self.set_state(LifecycleState::Failed);
        }
        self.exit.send_replace(Some(reason));
    }

    async fn lifecycle(
        &mut self,
        rt: Arc<RuntimeInner>,
        app: &LoadedApp,
        spec: &InstanceSpec,
    ) -> ExitReason {
        if let Some(code) = app.native() {
            let code = code.clone();
            return Box::pin(self.native(rt, &code, spec)).await;
        }
        let engine = rt.engine.clone();
        let weak = Arc::downgrade(&rt);
        let rt_slots = rt.clone();
        let ctx = ProcessContext {
            rt,
            me: self.me.clone(),
            mailbox: self.self_mailbox.clone(),
            dropped: self.dropped.clone(),
        };
        let state = match InstanceState::new(self.pid, spec, ctx) {
            Ok(state) => state,
            Err(why) => return ExitReason::Crashed(why),
        };
        let mut slot = match self.take_slot(&rt_slots, spec.init_timeout).await {
            Ok(slot) => slot,
            Err(no) => return no.reason(),
        };
        let mut store = new_store(&engine, state);
        let mut spec = spec.clone();

        let mut instance = match self.start(&mut store, app, &spec).await {
            Ok(instance) => instance,
            Err(reason) => return reason,
        };
        self.set_state(LifecycleState::Active);
        self.announce(&store);
        // Ends with the instance, or when this incarnation's lifecycle does.
        let _health = spec
            .health
            .map(|check| AbortOnDrop(tokio::spawn(health::watch(weak, self.me.clone(), check))));

        let stop = loop {
            match self.serve(&mut store, &mut instance, &mut spec).await {
                Ok(Served::Stop(stop)) => break stop,
                Err(reason) => return reason,
                Ok(Served::Idle) => {}
            }
            let snapshot = match self.save(&mut store, &instance, &spec).await {
                Ok(snapshot) => snapshot,
                Err(reason) => return reason,
            };
            if self.has_work() {
                // Something arrived during `save-snapshot`: stay live.
                continue;
            }
            let state = store.into_data();
            drop(slot);
            self.me.memory_counter().store(0, Ordering::Relaxed);
            self.set_state(LifecycleState::Passive);
            self.stats.passivated();
            tracing::debug!(bytes = snapshot.len(), "passivated");
            if let Some(reason) = self.passive(&snapshot).await {
                return reason;
            }
            let started = std::time::Instant::now();
            slot = match self.take_slot(&rt_slots, spec.init_timeout).await {
                Ok(slot) => slot,
                Err(no) => return no.reason(),
            };
            store = new_store(&engine, state);
            instance = match self.wake(&mut store, app, &spec, &snapshot).await {
                Ok(instance) => instance,
                Err(reason) => return reason,
            };
            self.stats.woke(started.elapsed());
            tracing::debug!(wake_us = started.elapsed().as_micros(), "woken");
        };

        self.set_state(LifecycleState::Stopping);
        self.mailbox.close();
        while self.mailbox.try_recv().is_ok() {}
        self.stop(&mut store, &instance, &spec, stop).await
    }

    /// Instantiate and run `init` within the init timeout, or, for the
    /// first instance of a singleton moved here with its state
    /// ([`crate::handover`]), `load-snapshot` with the moved snapshot. A
    /// refused or failed load ends the instance, and its supervisor's
    /// restart runs `init`.
    async fn start(
        &mut self,
        store: &mut Store<InstanceState>,
        app: &LoadedApp,
        spec: &InstanceSpec,
    ) -> Result<PaddockApp, ExitReason> {
        if let Some(stop) = self.await_lease(store.data().lease.clone()).await {
            return Err(stop);
        }
        let polls = store.data_mut().clock.start(spec.init_timeout);
        // Boxed, as are the other instantiations: their futures are large,
        // and the instance's task would otherwise keep room for them for
        // its whole life (passivated too).
        let handover = store.data().ctx.rt.handovers.take(&spec.app);
        let moved = handover.is_some();
        if let Some(h) = &handover {
            tracing::info!(
                from_version = %h.version,
                bytes = h.snapshot.len(),
                "starting from a moved snapshot"
            );
        }
        let init = tokio::time::timeout(
            spec.init_timeout,
            Box::pin(async {
                let instance = app.component()?.instantiate_async(&mut *store).await?;
                let result = match &handover {
                    Some(h) => {
                        instance
                            .call_load_snapshot(&mut *store, &h.version, &h.snapshot)
                            .await?
                    }
                    None => instance.call_init(&mut *store, &spec.init_args).await?,
                };
                wasmtime::Result::Ok((instance, result))
            }),
        );
        let crashed = |why: &str| ExitReason::Crashed(why.to_string());
        match killable(&self.kill, &self.control, Some(&polls), init).await {
            Err(reason) => Err(reason),
            Ok(Err(_elapsed)) => Err(crashed("init_timeout")),
            Ok(Ok(Err(err))) if store.data().clock.timed_out => {
                tracing::debug!(error = %describe(&err), "init ran out of time");
                Err(crashed("init_timeout"))
            }
            Ok(Ok(Err(err))) => Err(classify(&err, store.data())),
            Ok(Ok(Ok((_, Err(msg))))) if moved => Err(ExitReason::Crashed(format!(
                "handover: load-snapshot refused: {msg}"
            ))),
            Ok(Ok(Ok((_, Err(msg))))) => Err(ExitReason::Crashed(format!("init failed: {msg}"))),
            Ok(Ok(Ok((instance, Ok(()))))) => Ok(instance),
        }
    }

    /// Handle messages until a stop is requested (`Stop` with the reason),
    /// the instance has had nothing to do for `passivate_after` and may be
    /// passivated (`Idle`), or it fails (`Err`).
    async fn serve(
        &mut self,
        store: &mut Store<InstanceState>,
        instance: &mut PaddockApp,
        spec: &mut InstanceSpec,
    ) -> Result<Served, ExitReason> {
        loop {
            match self.control.borrow_and_update().clone() {
                Control::Run => {}
                Control::Shutdown(reason) => return Ok(Served::Stop(reason)),
                Control::Kill(reason) => return Err(reason),
            }
            let message = match self.receive(store, instance, spec).await {
                Next::Message(message) => message,
                Next::Again => continue,
                Next::Done(served) => return served,
            };
            // Out of lease, the message waits here; a stop drops it.
            if let Some(stop) = self.await_lease(store.data().lease.clone()).await {
                return match *self.control.borrow() {
                    Control::Shutdown(_) => Ok(Served::Stop(stop)),
                    Control::Run | Control::Kill(_) => Err(stop),
                };
            }

            let polls = store.data_mut().clock.start(spec.callback_deadline);
            let kind = match &message {
                Message::Call { .. } => "call",
                Message::Cast(_) => "cast",
                Message::Info(_) => "info",
            };
            probe::callback_start(self.pid, kind);
            let outcome = match message {
                Message::Call { msg, reply } => {
                    let call_ref = self.next_ref;
                    self.next_ref += 1;
                    store.data_mut().await_reply(call_ref, reply);
                    let msg = msg.into();
                    let call = instance.call_handle_call(&mut *store, call_ref, &msg);
                    match killable(&self.kill, &self.control, Some(&polls), call).await? {
                        Ok(CallResult::Reply(envelope)) => {
                            // Already answered with `process.reply`, or the
                            // caller timed out: the reply is dropped.
                            if let Some(waiting) = store.data_mut().pending.remove(&call_ref) {
                                let _ = waiting.send(envelope.into());
                            }
                            Ok(())
                        }
                        // The caller stays pending until `process.reply`.
                        Ok(CallResult::Noreply) => store.data().check_pending(),
                        Err(err) => Err(err),
                    }
                }
                Message::Cast(msg) => {
                    let msg = msg.into();
                    let cast = instance.call_handle_cast(&mut *store, &msg);
                    killable(&self.kill, &self.control, Some(&polls), cast).await?
                }
                Message::Info(info) => {
                    if let Info::Timer { reference, .. } = &info {
                        store.data_mut().timers.fired(*reference);
                    }
                    let info = info.into();
                    let handle = instance.call_handle_info(&mut *store, &info);
                    killable(&self.kill, &self.control, Some(&polls), handle).await?
                }
            };
            let guest = store.data().clock.guest_time();
            self.stats.callback(kind, guest);
            if probe::callback_done_enabled() {
                let guest_ns = u64::try_from(guest.as_nanos()).unwrap_or(u64::MAX);
                probe::callback_done(self.pid, kind, guest_ns, outcome.is_ok());
            }
            if let Err(err) = outcome {
                tracing::debug!(error = %describe(&err), "callback failed");
                return Err(classify(&err, store.data()));
            }
        }
    }

    /// The next message to handle, after any operation or signal that
    /// comes first; `Done` once a stop is requested, the mailbox closes or
    /// the instance may be passivated.
    async fn receive(
        &mut self,
        store: &mut Store<InstanceState>,
        instance: &mut PaddockApp,
        spec: &mut InstanceSpec,
    ) -> Next {
        if let Some(op) = self.held_op.take() {
            return self.run_op(store, instance, spec, op).await;
        }
        if let Some(message) = self.held.take() {
            return Next::Message(message);
        }
        let idle = spec
            .passivate_after
            .filter(|_| store.data_mut().passivable());
        // Asked to passivate early, while idle, for an instance waiting
        // for a Store slot.
        let rt = idle.map(|_| store.data().ctx.rt.clone());
        let wanted = async {
            match &rt {
                Some(rt) => rt.slot_wanted.notified().await,
                None => std::future::pending().await,
            }
        };
        let (waits, table) = store.data_mut().ready_parts();
        let ready = crate::ready::next(waits, table);
        let woke = tokio::select! {
            biased;
            changed = self.control.changed() => {
                if changed.is_err() {
                    return Next::Done(Err(ExitReason::Killed));
                }
                Woke::Next(Next::Again)
            }
            Some(op) = self.ops.recv() => Woke::Op(op),
            Some(info) = self.signals.recv() => Woke::Next(match info.resolve() {
                Some(info) => Next::Message(Message::Info(info)),
                // A withdrawn notice (an ended watch).
                None => Next::Again,
            }),
            // Ahead of the mailbox: a notice is sent once per `notify`, so
            // the guest decides how often it comes.
            event = ready => Woke::Next(match event {
                Some((name, msg)) => Next::Message(Message::Info(Info::HostEvent { name, msg })),
                None => Next::Again,
            }),
            message = self.mailbox.recv() => Woke::Next(match message {
                Some(message) => Next::Message(message),
                None => Next::Done(Ok(Served::Stop(ExitReason::Shutdown))),
            }),
            () = tokio::time::sleep(idle.unwrap_or_default()), if idle.is_some() => {
                Woke::Next(Next::Done(Ok(Served::Idle)))
            }
            () = wanted, if idle.is_some() => Woke::Next(Next::Done(Ok(Served::Idle))),
        };
        match woke {
            Woke::Next(next) => next,
            Woke::Op(op) => self.run_op(store, instance, spec, op).await,
        }
    }

    /// Run a host operation once the lease allows guest code.
    async fn run_op(
        &mut self,
        store: &mut Store<InstanceState>,
        instance: &mut PaddockApp,
        spec: &mut InstanceSpec,
        op: Box<Op>,
    ) -> Next {
        if let Some(stop) = self.await_lease(store.data().lease.clone()).await {
            return Next::Done(Err(stop));
        }
        match self.operate(store, instance, spec, *op).await {
            Some(reason) => Next::Done(Err(reason)),
            None => Next::Again,
        }
    }

    /// Run a host operation between callbacks. Returns the exit reason if
    /// the instance cannot continue.
    async fn operate(
        &mut self,
        store: &mut Store<InstanceState>,
        instance: &mut PaddockApp,
        spec: &mut InstanceSpec,
        op: Op,
    ) -> Option<ExitReason> {
        match op {
            Op::Upgrade {
                app,
                spec: next,
                from_version,
                reply,
            } => {
                let result = self
                    .upgrade(store, instance, spec, &app, *next, &from_version)
                    .await;
                let exit = match &result {
                    Err(UpgradeError::SaveFailed(reason)) => Some(reason.clone()),
                    _ => None,
                };
                let _ = reply.send(result);
                exit
            }
            Op::Snapshot { reply } => {
                let (result, exit) = self.save_now(store, instance, spec).await;
                let _ = reply.send(result);
                exit
            }
            Op::MoveOut { reply } => {
                let (result, exit) = self.save_now(store, instance, spec).await;
                let moved = result.is_ok();
                let _ = reply.send(result);
                match exit {
                    Some(reason) => Some(reason),
                    None if moved => Some(self.frozen().await),
                    None => None,
                }
            }
            Op::Announce => {
                self.announce(store);
                None
            }
        }
    }

    /// `save-snapshot` now, for an operation: the snapshot, and the exit
    /// reason if the instance cannot continue.
    async fn save_now(
        &mut self,
        store: &mut Store<InstanceState>,
        instance: &PaddockApp,
        spec: &InstanceSpec,
    ) -> (Result<Vec<u8>, SnapshotError>, Option<ExitReason>) {
        let polls = store.data_mut().clock.start(spec.callback_deadline);
        let saved = killable(
            &self.kill,
            &self.control,
            Some(&polls),
            instance.call_save_snapshot(&mut *store),
        )
        .await;
        match saved {
            Ok(Ok(bytes)) => (Ok(bytes), None),
            Ok(Err(err)) => {
                let reason = classify(&err, store.data());
                (Err(SnapshotError::Failed(reason.clone())), Some(reason))
            }
            Err(killed) => (Err(SnapshotError::Failed(killed.clone())), Some(killed)),
        }
    }

    /// Moved out: handle nothing more, and end at the first stop without
    /// `terminate`: `normal` for a graceful one, the kill's reason for a
    /// kill.
    async fn frozen(&mut self) -> ExitReason {
        loop {
            match self.control.borrow_and_update().clone() {
                Control::Run => {}
                Control::Shutdown(_) => return ExitReason::Normal,
                Control::Kill(reason) => return reason,
            }
            if self.control.changed().await.is_err() {
                return ExitReason::Killed;
            }
        }
    }

    /// Queue the services' host events, ahead of mailbox messages.
    fn announce(&self, store: &Store<InstanceState>) {
        for (name, msg) in store.data().announcements() {
            self.me.signal(Info::HostEvent { name, msg });
        }
    }

    /// Snapshot upgrade: `save-snapshot` on the current version, then a new
    /// Store with the new component and `load-snapshot`. The current Store
    /// is untouched until the new one has loaded, so any failure after the
    /// snapshot keeps the old version running. A kill during the upgrade
    /// returns `SaveFailed` with the kill's reason, which ends the instance.
    /// The new Store takes a Store slot of its own while both exist; with
    /// none free within the new version's `init_timeout`, the upgrade
    /// fails before the snapshot and the old version keeps running.
    async fn upgrade(
        &mut self,
        store: &mut Store<InstanceState>,
        instance: &mut PaddockApp,
        spec: &mut InstanceSpec,
        app: &LoadedApp,
        next: InstanceSpec,
        from_version: &str,
    ) -> Result<Duration, UpgradeError> {
        let started = std::time::Instant::now();
        let rt = store.data().ctx.rt.clone();
        // Dropped at the end: by then the old Store is gone if the new one
        // replaced it, and the instance's own slot holds the new one.
        let _second = match self.take_slot(&rt, next.init_timeout).await {
            Ok(slot) => slot,
            Err(NoSlot::Killed(reason)) => return Err(UpgradeError::SaveFailed(reason)),
            Err(NoSlot::Timeout) => {
                return Err(UpgradeError::Instantiate("no instance slot".into()));
            }
        };
        self.set_state(LifecycleState::Upgrading);
        let polls = store.data_mut().clock.start(spec.callback_deadline);
        let saved = killable(
            &self.kill,
            &self.control,
            Some(&polls),
            instance.call_save_snapshot(&mut *store),
        )
        .await;
        let snapshot = match saved {
            Err(killed) => return Err(UpgradeError::SaveFailed(killed)),
            Ok(Err(err)) => return Err(UpgradeError::SaveFailed(classify(&err, store.data()))),
            Ok(Ok(bytes)) => bytes,
        };

        let mut state = match InstanceState::new(self.pid, &next, store.data().context()) {
            Ok(state) => state,
            Err(why) => {
                self.set_state(LifecycleState::Active);
                return Err(UpgradeError::Instantiate(why));
            }
        };
        store.data_mut().hand_over(&mut state);
        let mut fresh = new_store(store.engine(), state);
        let polls = fresh.data_mut().clock.start(next.init_timeout);
        let load = tokio::time::timeout(
            next.init_timeout,
            Box::pin(async {
                let new_instance = match app.component() {
                    Ok(pre) => pre.instantiate_async(&mut fresh).await,
                    Err(e) => Err(e),
                };
                match new_instance {
                    Err(err) => Err(UpgradeError::Instantiate(describe(&err))),
                    Ok(new_instance) => match new_instance
                        .call_load_snapshot(&mut fresh, from_version, &snapshot)
                        .await
                    {
                        Ok(Ok(())) => Ok(new_instance),
                        Ok(Err(refused)) => Err(UpgradeError::LoadFailed(refused)),
                        Err(err) => Err(UpgradeError::LoadFailed(describe(&err))),
                    },
                }
            }),
        );
        let outcome = match killable(&self.kill, &self.control, Some(&polls), load).await {
            Err(killed) => Err(UpgradeError::SaveFailed(killed)),
            Ok(Err(_elapsed)) => Err(UpgradeError::LoadFailed("timed out".into())),
            Ok(Ok(result)) => result,
        };
        match outcome {
            Ok(new_instance) => {
                *store = fresh;
                *instance = new_instance;
                self.me
                    .shared
                    .message_max_bytes
                    .store(next.message_max_bytes, Ordering::Relaxed);
                *self
                    .me
                    .shared
                    .acl
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Arc::new(next.acl.clone());
                *spec = next;
                *self
                    .me
                    .shared
                    .version
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = spec.version.clone();
                self.set_state(LifecycleState::Active);
                // The new guest starts with fresh statics.
                self.announce(store);
                let pause = started.elapsed();
                tracing::info!(version = %spec.version, pause_us = pause.as_micros(), "upgraded");
                Ok(pause)
            }
            Err(err) => {
                // Give the instance's timers and waiting callers back.
                fresh.data_mut().hand_over(store.data_mut());
                if !matches!(err, UpgradeError::SaveFailed(_)) {
                    self.set_state(LifecycleState::Active);
                }
                tracing::warn!(error = %err, "upgrade reverted");
                Err(err)
            }
        }
    }

    /// A Store slot, waiting up to `within` for one if every slot is taken
    /// (only with passivation, which lets more instances exist than slots);
    /// an idle instance is asked to passivate early to free one.
    async fn take_slot(
        &mut self,
        rt: &RuntimeInner,
        within: Duration,
    ) -> Result<OwnedSemaphorePermit, NoSlot> {
        let slots: &Arc<Semaphore> = &rt.slots;
        if let Ok(slot) = slots.clone().try_acquire_owned() {
            return Ok(slot);
        }
        let deadline = tokio::time::Instant::now() + within;
        // Ask again until a slot frees: a request made while no idle
        // instance was listening is kept only once, however many there were.
        let acquire = slots.clone().acquire_owned();
        tokio::pin!(acquire);
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                self.stats.slot_timeout();
                tracing::warn!(
                    wait_ms = within.as_millis(),
                    "no Store slot freed in time: every slot is held by instances that \
                     cannot passivate"
                );
                return Err(NoSlot::Timeout);
            }
            rt.slot_wanted.notify_one();
            let waited = tokio::time::timeout(SLOT_ASK_AGAIN.min(deadline - now), &mut acquire);
            match killable(&self.kill, &self.control, None, waited).await {
                Err(killed) => return Err(NoSlot::Killed(killed)),
                Ok(Ok(Ok(slot))) => return Ok(slot),
                // The semaphore is never closed.
                Ok(Ok(Err(_closed))) => return Err(NoSlot::Killed(ExitReason::Killed)),
                Ok(Err(_elapsed)) => {}
            }
        }
    }

    /// `save-snapshot`, within the callback deadline. A failure ends the
    /// instance, as in an upgrade.
    async fn save(
        &mut self,
        store: &mut Store<InstanceState>,
        instance: &PaddockApp,
        spec: &InstanceSpec,
    ) -> Result<Vec<u8>, ExitReason> {
        let polls = store.data_mut().clock.start(spec.callback_deadline);
        match killable(
            &self.kill,
            &self.control,
            Some(&polls),
            instance.call_save_snapshot(&mut *store),
        )
        .await?
        {
            Ok(bytes) => Ok(bytes),
            Err(err) => Err(classify(&err, store.data())),
        }
    }

    /// Whether a message, signal or operation is waiting.
    fn has_work(&self) -> bool {
        !self.mailbox.is_empty() || !self.signals.is_empty() || !self.ops.is_empty()
    }

    /// Wait, passivated, for something to wake the instance; it is kept in
    /// `held` or `held_op`. A `snapshot` operation is answered from
    /// `snapshot` without waking. Returns the reason of a kill, which ends
    /// the instance without waking it; a graceful stop wakes it to run
    /// `terminate`.
    async fn passive(&mut self, snapshot: &[u8]) -> Option<ExitReason> {
        loop {
            match self.control.borrow_and_update().clone() {
                Control::Run => {}
                Control::Shutdown(_) => return None,
                Control::Kill(reason) => return Some(reason),
            }
            tokio::select! {
                biased;
                changed = self.control.changed() => if changed.is_err() {
                    return Some(ExitReason::Killed);
                },
                Some(op) = self.ops.recv() => match *op {
                    Op::Snapshot { reply } => {
                        let _ = reply.send(Ok(snapshot.to_vec()));
                    }
                    Op::MoveOut { reply } => {
                        let _ = reply.send(Ok(snapshot.to_vec()));
                        return Some(self.frozen().await);
                    }
                    op => {
                        self.held_op = Some(Box::new(op));
                        return None;
                    }
                },
                Some(info) = self.signals.recv() => if let Some(info) = info.resolve() {
                    self.held = Some(Message::Info(info));
                    return None;
                },
                // `None`: the mailbox closed, and `serve` stops.
                message = self.mailbox.recv() => {
                    self.held = message;
                    return None;
                }
            }
        }
    }

    /// Instantiate the component on a Store built from the kept data and
    /// run `load-snapshot(own version, snapshot)` within the init timeout.
    /// A failure ends the instance: its state is lost.
    async fn wake(
        &mut self,
        store: &mut Store<InstanceState>,
        app: &LoadedApp,
        spec: &InstanceSpec,
        snapshot: &[u8],
    ) -> Result<PaddockApp, ExitReason> {
        if let Some(stop) = self.await_lease(store.data().lease.clone()).await {
            return Err(stop);
        }
        let polls = store.data_mut().clock.start(spec.init_timeout);
        let load = tokio::time::timeout(
            spec.init_timeout,
            Box::pin(async {
                let instance = app.component()?.instantiate_async(&mut *store).await?;
                let result = instance
                    .call_load_snapshot(&mut *store, &spec.version, snapshot)
                    .await?;
                wasmtime::Result::Ok((instance, result))
            }),
        );
        let crashed = |why: &str| ExitReason::Crashed(why.to_string());
        let instance = match killable(&self.kill, &self.control, Some(&polls), load).await? {
            Err(_elapsed) => return Err(crashed("wake_timeout")),
            Ok(Err(_)) if store.data().clock.timed_out => return Err(crashed("wake_timeout")),
            Ok(Err(err)) => return Err(classify(&err, store.data())),
            Ok(Ok((_, Err(msg)))) => {
                return Err(ExitReason::Crashed(format!(
                    "wake: load-snapshot refused: {msg}"
                )));
            }
            Ok(Ok((instance, Ok(())))) => instance,
        };
        self.set_state(LifecycleState::Active);
        // The new guest starts with fresh statics, as after an upgrade.
        self.announce(store);
        Ok(instance)
    }

    /// Wait until guest code may run: the instance's lease, if it is bound
    /// to one, is current. Returns the exit reason if the instance is
    /// stopped or killed first (the stop's reason or the kill's), or if
    /// the runtime is gone.
    async fn await_lease(
        &mut self,
        lease: Option<watch::Receiver<RunUntil>>,
    ) -> Option<ExitReason> {
        let mut lease = lease?;
        let mut logged = false;
        loop {
            if lease
                .borrow_and_update()
                .allows(tokio::time::Instant::now())
            {
                return None;
            }
            if !logged {
                tracing::info!("singleton out of lease: waiting to run guest code");
                logged = true;
            }
            match &*self.control.borrow_and_update() {
                Control::Run => {}
                Control::Shutdown(reason) | Control::Kill(reason) => return Some(reason.clone()),
            }
            tokio::select! {
                changed = self.control.changed() => if changed.is_err() {
                    return Some(ExitReason::Killed);
                },
                changed = lease.changed() => if changed.is_err() {
                    return Some(ExitReason::Killed);
                },
            }
        }
    }

    /// Run `terminate` within the shutdown timeout. Overrunning it is a
    /// kill, and so is a stop while out of lease: `terminate` is guest code.
    async fn stop(
        &mut self,
        store: &mut Store<InstanceState>,
        instance: &PaddockApp,
        spec: &InstanceSpec,
        reason: ExitReason,
    ) -> ExitReason {
        if !store.data().within_lease() {
            tracing::info!(%reason, "singleton out of lease: terminate skipped");
            return ExitReason::Killed;
        }
        let polls = store.data_mut().clock.start(spec.shutdown_timeout);
        let wit_reason = (&reason).into();
        let terminate = tokio::time::timeout(
            spec.shutdown_timeout,
            instance.call_terminate(&mut *store, &wit_reason),
        );
        match killable(&self.kill, &self.control, Some(&polls), terminate).await {
            Err(killed) => killed,
            Ok(Err(_elapsed)) => ExitReason::Killed,
            Ok(Ok(Err(_))) if store.data().clock.timed_out => ExitReason::Killed,
            Ok(Ok(Err(err))) => classify(&err, store.data()),
            Ok(Ok(Ok(()))) => reason,
        }
    }

    fn set_state(&self, next: LifecycleState) {
        let current = *self.state.borrow();
        if !current.can_transition_to(next) {
            tracing::warn!(?current, ?next, "unexpected lifecycle transition");
        }
        self.state.send_replace(next);
    }
}

/// A Store with the instance's limits, deadline accounting and preemption.
pub(crate) fn new_store(engine: &wasmtime::Engine, state: InstanceState) -> Store<InstanceState> {
    let mut store = Store::new(engine, state);
    store.limiter(|s| &mut s.limiter);
    store.epoch_deadline_callback(|mut ctx| {
        if ctx.data_mut().clock.expired() {
            Ok(UpdateDeadline::Interrupt)
        } else if !ctx.data().within_lease() {
            // The lease ended while guest code ran: stop it now (A8 in
            // docs/tla/VERIFICATION.md).
            ctx.data_mut().lease_ended = true;
            Ok(UpdateDeadline::Interrupt)
        } else {
            // Cooperative preemption: give other instances the thread.
            Ok(UpdateDeadline::Yield(1))
        }
    });
    store.set_epoch_deadline(1);
    store
}

/// Run `fut` unless the instance is killed first; `Err` carries the kill's
/// exit reason, and `fut` was dropped. With `polls`, the time spent polling
/// `fut` is the callback's guest time: here rather than in a future of its
/// own, which would move the large callback future once more.
async fn killable<F: Future>(
    kill: &KillSignal,
    control: &watch::Receiver<Control>,
    polls: Option<&PollTime>,
    fut: F,
) -> Result<F::Output, ExitReason> {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| {
        // Registered before the check, so a kill between the two still
        // wakes this task, as one while `fut` waits in a host call does.
        kill.waker.register(cx.waker());
        if kill.killed.load(Ordering::Acquire) {
            return Poll::Ready(Err(match &*control.borrow() {
                Control::Kill(reason) => reason.clone(),
                _ => ExitReason::Killed,
            }));
        }
        match polls {
            Some(polls) => polls.poll(fut.as_mut(), cx),
            None => fut.as_mut().poll(cx),
        }
        .map(Ok)
    })
    .await
}

/// Set once with the first kill, after the control channel says
/// `Control::Kill`, and woken: what [`killable`] checks on each poll. A
/// `watch` future per callback cost about 10% of an empty call
/// (`call_split`, ROADMAP B12).
#[derive(Debug, Default)]
struct KillSignal {
    killed: AtomicBool,
    waker: AtomicWaker,
}

/// Map a failed guest call to an exit reason.
#[path = "instance_native.rs"]
mod native;

pub(crate) fn classify(err: &wasmtime::Error, state: &InstanceState) -> ExitReason {
    if state.limiter.memory_exceeded {
        return ExitReason::Crashed("memory_limit".to_string());
    }
    if state.limiter.table_exceeded {
        return ExitReason::Crashed("table_limit".to_string());
    }
    if state.lease_ended {
        return ExitReason::Crashed("lease_ended".to_string());
    }
    let trap = err.downcast_ref::<Trap>();
    if state.clock.timed_out || trap == Some(&Trap::Interrupt) {
        return ExitReason::Timeout;
    }
    let why = match trap {
        Some(trap) => format!("trap: {trap}"),
        None => err
            .root_cause()
            .to_string()
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
    };
    ExitReason::Crashed(why)
}
