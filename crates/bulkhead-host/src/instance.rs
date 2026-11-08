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
use tokio::sync::{mpsc, oneshot, watch};
use tracing::Instrument;

use crate::acl::Acl;
use crate::delivery;
use crate::failover::RunUntil;
use crate::lifecycle::LifecycleState;
use crate::mailbox::{self, WaitError};
use crate::message::{CallError, Envelope, ExitReason, Info};
use crate::metrics::AppStats;
use crate::pid::Pid;
use crate::probe;
use crate::runtime::{LoadedApp, RuntimeInner};
use crate::spec::InstanceSpec;
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
    /// Take a connection a listener accepted (a native app's
    /// `handle_connection`); a component drops it, closing it.
    Connection(crate::listener::Accepted),
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
    /// A rustbridge plugin takes no snapshot (a native executable does,
    /// over the protocol).
    #[error("a rustbridge plugin takes no snapshot")]
    NotSupported,
}

/// How [`InstanceTask::serve`] ended without a failure.
enum Served {
    /// A stop was requested, with this reason.
    Stop(ExitReason),
    /// Nothing to do for `passivate_after`.
    Idle,
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
    mailbox: mailbox::Sender,
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
    /// Whether the callback running now sent something that woke another
    /// task (a cast, a reply, an exit signal, a publish).
    woke: AtomicBool,
    /// Bytes of linear memory, as the Store's limiter last allowed it to
    /// grow.
    memory: Arc<AtomicU64>,
    /// Where a native instance's memory is read, while it has a process.
    native_memory: std::sync::Mutex<Option<crate::native::memory::MemorySource>>,
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
        self.shared.mailbox.queued()
    }

    /// Bytes of message bodies waiting in the mailbox
    /// (`mailbox_max_bytes` bounds them).
    #[must_use]
    pub fn queued_bytes(&self) -> u64 {
        self.shared.mailbox.queued_bytes()
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

    /// Bytes of memory the instance holds. A component: the linear memory
    /// it has grown to (its largest as last grown). A native app: its
    /// cgroup's `memory.current`, or else the resident set of its process
    /// and that process's direct children, read now. 0 before it starts and
    /// while it is passivated.
    #[must_use]
    pub fn memory_bytes(&self) -> u64 {
        let source = self
            .shared
            .native_memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        source.map_or_else(
            || self.shared.memory.load(Ordering::Relaxed),
            |source| source.read(),
        )
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

    /// Where to read this native instance's memory from now: its current
    /// process, or `None` while it has none.
    pub(crate) fn set_native_memory(&self, source: Option<crate::native::memory::MemorySource>) {
        *self
            .shared
            .native_memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = source;
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

    /// [`InstanceHandle::cast_from`] that waits up to `timeout` for room
    /// in a full mailbox (a native app's `cast_timeout`, ROADMAP I2).
    /// [`CallError::Timeout`] means the cast was not queued; it is counted
    /// as dropped, as a cast refused at a full mailbox is.
    pub(crate) async fn cast_wait_from(
        &self,
        from: Option<Pid>,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<(), CallError> {
        let bytes = msg.body.len();
        let result = match self.check_size(&msg) {
            Err(e) => Err(e),
            Ok(()) => self
                .shared
                .mailbox
                .send_timeout(Message::Cast(msg), timeout)
                .await
                .map_err(|e| {
                    if e == WaitError::Closed {
                        return CallError::Noproc;
                    }
                    self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                    probe::mailbox_drop(self.pid(), "cast");
                    if e == WaitError::Timeout {
                        CallError::Timeout
                    } else {
                        CallError::Overloaded
                    }
                }),
        };
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

    /// Hand the instance a connection its app's listener accepted, ahead
    /// of its mailbox. Ignored (the connection closes) once it has exited.
    pub(crate) fn connection(&self, accepted: crate::listener::Accepted) {
        let _ = self.shared.ops.send(Box::new(Op::Connection(accepted)));
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

    /// Note that this instance's callback sent a message that woke another
    /// task, which the instance then moves off its worker's LIFO slot
    /// (ROADMAP B15).
    pub(crate) fn woke_others(&self) {
        self.shared.woke.store(true, Ordering::Relaxed);
    }

    /// Whether the callback just run woke another task, clearing it.
    fn take_woke(&self) -> bool {
        self.shared.woke.swap(false, Ordering::Relaxed)
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
    mailbox: mailbox::Receiver,
    signals: mpsc::UnboundedReceiver<Info>,
    ops: mpsc::UnboundedReceiver<Box<Op>>,
    self_mailbox: mailbox::WeakSender,
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
    /// The code the instance runs, once started: what a wake from
    /// passivation instantiates. A successful upgrade replaces it, so the
    /// old version's code and memory image go with its Store.
    app: Option<LoadedApp>,
    /// The next call reference, unique for the instance's whole life: a
    /// guest woken from a snapshot may still hold the reference of a call
    /// whose caller gave up.
    next_ref: u64,
    /// A native instance's callback deadline: one timer for all its
    /// callbacks, re-armed when it fires early, so a callback arms none
    /// (Z1).
    native_timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    /// Processes a native instance has started, for each one's cgroup
    /// name: an upgrade starts the new one beside the old.
    native_spawns: u32,
    /// The published buckets a native instance reads (ROADMAP T4).
    native_published: Option<crate::services::keyvalue::publish::Publications>,
}

/// Create the channels for a new instance.
pub(crate) fn prepare(
    pid: Pid,
    spec: &InstanceSpec,
    stats: Arc<AppStats>,
) -> (InstanceHandle, InstanceTask) {
    let (mailbox_tx, mailbox_rx) = mailbox::channel(spec.mailbox_max, spec.mailbox_max_bytes);
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
            woke: AtomicBool::new(false),
            memory: Arc::new(AtomicU64::new(0)),
            native_memory: std::sync::Mutex::new(None),
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
        app: None,
        native_timer: None,
        native_spawns: 0,
        native_published: None,
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
            .lifecycle(rt.clone(), app, &spec)
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
                Op::Announce | Op::Connection(_) => {}
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
        app: LoadedApp,
        spec: &InstanceSpec,
    ) -> ExitReason {
        let code = app.native().clone();
        // What a wake from passivation starts; an upgrade replaces it.
        self.app = Some(app);
        Box::pin(self.native(rt, &code, spec)).await
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

    /// Take on an upgraded version's message limit, messaging ACL and
    /// version, as the handle reports them. A higher mailbox byte limit
    /// applies at once, a lower one at the next start, as `mailbox_max`
    /// does.
    fn take_spec(&self, next: &InstanceSpec) {
        self.me
            .shared
            .message_max_bytes
            .store(next.message_max_bytes, Ordering::Relaxed);
        self.me
            .shared
            .mailbox
            .raise_max_bytes(next.mailbox_max_bytes);
        *self
            .me
            .shared
            .acl
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(next.acl.clone());
        *self
            .me
            .shared
            .version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next.version.clone();
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

    fn set_state(&self, next: LifecycleState) {
        let current = *self.state.borrow();
        if !current.can_transition_to(next) {
            tracing::warn!(?current, ?next, "unexpected lifecycle transition");
        }
        self.state.send_replace(next);
    }
}

/// Run `fut` unless the instance is killed first; `Err` carries the kill's
/// exit reason, and `fut` was dropped.
async fn killable<F: Future>(
    kill: &KillSignal,
    control: &watch::Receiver<Control>,
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
        fut.as_mut().poll(cx).map(Ok)
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
