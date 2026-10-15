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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
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
use crate::state::{InstanceState, ProcessContext};
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
    /// Send the services' current host events again.
    Announce,
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
    state: watch::Receiver<LifecycleState>,
    exit: watch::Receiver<Option<ExitReason>>,
    message_max_bytes: AtomicU64,
    /// Which apps it may send to and accept from; replaced at an upgrade.
    acl: std::sync::RwLock<Arc<Acl>>,
    dropped: Arc<AtomicU64>,
    ops: mpsc::UnboundedSender<Op>,
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
        tokio::time::timeout(timeout, waiting)
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
            .send(Op::Upgrade {
                app: app.clone(),
                spec: Box::new(spec),
                from_version: from_version.to_string(),
                reply,
            })
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
            .send(Op::Snapshot { reply })
            .map_err(|_| SnapshotError::NotRunning)?;
        answer.await.unwrap_or(Err(SnapshotError::NotRunning))
    }

    /// Have the instance send its services' host events again, after an
    /// operator change such as a new log level. Ignored once it has exited.
    pub(crate) fn announce(&self) {
        let _ = self.shared.ops.send(Op::Announce);
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
        self.shared.control.send_if_modified(|c| {
            let first = !matches!(c, Control::Kill(_));
            if first {
                *c = Control::Kill(reason);
            }
            first
        });
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
    ops: mpsc::UnboundedReceiver<Op>,
    self_mailbox: mpsc::WeakSender<Message>,
    dropped: Arc<AtomicU64>,
    control: watch::Receiver<Control>,
    state: watch::Sender<LifecycleState>,
    exit: watch::Sender<Option<ExitReason>>,
    /// The app's host counters.
    stats: Arc<AppStats>,
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
        state: state_tx,
        exit: exit_tx,
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
            match op {
                Op::Upgrade { reply, .. } => {
                    let _ = reply.send(Err(UpgradeError::NotRunning));
                }
                Op::Snapshot { reply } => {
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
        let engine = rt.engine.clone();
        let weak = Arc::downgrade(&rt);
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

        let stop = match self.serve(&mut store, &mut instance, &mut spec).await {
            Ok(stop) => stop,
            Err(reason) => return reason,
        };

        self.set_state(LifecycleState::Stopping);
        self.mailbox.close();
        while self.mailbox.try_recv().is_ok() {}
        self.stop(&mut store, &instance, &spec, stop).await
    }

    /// Instantiate and run `init` within the init timeout.
    async fn start(
        &mut self,
        store: &mut Store<InstanceState>,
        app: &LoadedApp,
        spec: &InstanceSpec,
    ) -> Result<PaddockApp, ExitReason> {
        if let Some(stop) = self.await_lease(store.data().lease.clone()).await {
            return Err(stop);
        }
        store.data_mut().clock.start(spec.init_timeout);
        let init = tokio::time::timeout(spec.init_timeout, async {
            let instance = app.pre.instantiate_async(&mut *store).await?;
            let result = instance.call_init(&mut *store, &spec.init_args).await?;
            wasmtime::Result::Ok((instance, result))
        });
        let crashed = |why: &str| ExitReason::Crashed(why.to_string());
        match killable(&mut self.control, init).await {
            Err(reason) => Err(reason),
            Ok(Err(_elapsed)) => Err(crashed("init_timeout")),
            Ok(Ok(Err(err))) if store.data().clock.timed_out => {
                tracing::debug!(error = %describe(&err), "init ran out of time");
                Err(crashed("init_timeout"))
            }
            Ok(Ok(Err(err))) => Err(classify(&err, store.data())),
            Ok(Ok(Ok((_, Err(msg))))) => Err(ExitReason::Crashed(format!("init failed: {msg}"))),
            Ok(Ok(Ok((instance, Ok(()))))) => Ok(instance),
        }
    }

    /// Handle messages until a stop is requested (`Ok` with the reason) or
    /// the instance fails (`Err`).
    async fn serve(
        &mut self,
        store: &mut Store<InstanceState>,
        instance: &mut PaddockApp,
        spec: &mut InstanceSpec,
    ) -> Result<ExitReason, ExitReason> {
        let mut next_ref: u64 = 1;
        loop {
            match self.control.borrow_and_update().clone() {
                Control::Run => {}
                Control::Shutdown(reason) => return Ok(reason),
                Control::Kill(reason) => return Err(reason),
            }
            let message = tokio::select! {
                biased;
                changed = self.control.changed() => {
                    if changed.is_err() {
                        return Err(ExitReason::Killed);
                    }
                    continue;
                }
                Some(op) = self.ops.recv() => {
                    if let Some(stop) = self.await_lease(store.data().lease.clone()).await {
                        return Err(stop);
                    }
                    if let Some(reason) = self.operate(store, instance, spec, op).await {
                        return Err(reason);
                    }
                    continue;
                }
                Some(info) = self.signals.recv() => match info.resolve() {
                    Some(info) => Some(Message::Info(info)),
                    // A withdrawn notice (an ended watch).
                    None => continue,
                },
                message = self.mailbox.recv() => message,
            };
            let Some(message) = message else {
                return Ok(ExitReason::Shutdown);
            };
            // Out of lease, the message waits here; a stop drops it.
            if let Some(stop) = self.await_lease(store.data().lease.clone()).await {
                return match *self.control.borrow() {
                    Control::Shutdown(_) => Ok(stop),
                    Control::Run | Control::Kill(_) => Err(stop),
                };
            }

            store.data_mut().clock.start(spec.callback_deadline);
            let kind = match &message {
                Message::Call { .. } => "call",
                Message::Cast(_) => "cast",
                Message::Info(_) => "info",
            };
            probe::callback_start(self.pid, kind);
            let outcome = match message {
                Message::Call { msg, reply } => {
                    let call_ref = next_ref;
                    next_ref += 1;
                    store.data_mut().await_reply(call_ref, reply);
                    let msg = msg.into();
                    let call = instance.call_handle_call(&mut *store, call_ref, &msg);
                    match killable(&mut self.control, call).await? {
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
                    killable(&mut self.control, cast).await?
                }
                Message::Info(info) => {
                    if let Info::Timer { reference, .. } = &info {
                        store.data_mut().timers.fired(*reference);
                    }
                    let info = info.into();
                    let handle = instance.call_handle_info(&mut *store, &info);
                    killable(&mut self.control, handle).await?
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
                store.data_mut().clock.start(spec.callback_deadline);
                let saved =
                    killable(&mut self.control, instance.call_save_snapshot(&mut *store)).await;
                let (result, exit) = match saved {
                    Ok(Ok(bytes)) => (Ok(bytes), None),
                    Ok(Err(err)) => {
                        let reason = classify(&err, store.data());
                        (Err(SnapshotError::Failed(reason.clone())), Some(reason))
                    }
                    Err(killed) => (Err(SnapshotError::Failed(killed.clone())), Some(killed)),
                };
                let _ = reply.send(result);
                exit
            }
            Op::Announce => {
                self.announce(store);
                None
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
        self.set_state(LifecycleState::Upgrading);
        store.data_mut().clock.start(spec.callback_deadline);
        let saved = killable(&mut self.control, instance.call_save_snapshot(&mut *store)).await;
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
        fresh.data_mut().clock.start(next.init_timeout);
        let load = tokio::time::timeout(next.init_timeout, async {
            let new_instance = app.pre.instantiate_async(&mut fresh).await;
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
        });
        let outcome = match killable(&mut self.control, load).await {
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
        store.data_mut().clock.start(spec.shutdown_timeout);
        let wit_reason = (&reason).into();
        let terminate = tokio::time::timeout(
            spec.shutdown_timeout,
            instance.call_terminate(&mut *store, &wit_reason),
        );
        match killable(&mut self.control, terminate).await {
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
/// exit reason, and `fut` was dropped.
async fn killable<F: Future>(
    control: &mut watch::Receiver<Control>,
    fut: F,
) -> Result<F::Output, ExitReason> {
    tokio::select! {
        biased;
        killed = control.wait_for(|c| matches!(c, Control::Kill(_))) => Err(match killed {
            Ok(c) => match &*c {
                Control::Kill(reason) => reason.clone(),
                _ => ExitReason::Killed,
            },
            // Every handle is gone; treat that as a kill.
            Err(_) => ExitReason::Killed,
        }),
        out = fut => Ok(out),
    }
}

/// Map a failed guest call to an exit reason.
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
