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

use crate::bindings::{App, CallResult};
use crate::error::describe;
use crate::lifecycle::LifecycleState;
use crate::message::{CallError, Envelope, ExitReason, Info};
use crate::pid::Pid;
use crate::runtime::{LoadedApp, RuntimeInner};
use crate::spec::InstanceSpec;
use crate::state::{InstanceState, ProcessContext};

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
    mailbox: mpsc::Sender<Message>,
    control: watch::Sender<Control>,
    state: watch::Receiver<LifecycleState>,
    exit: watch::Receiver<Option<ExitReason>>,
    message_max_bytes: u64,
    dropped: Arc<AtomicU64>,
    /// Exit and down notifications. Unbounded: OTP never drops signals, and
    /// their number is bounded by links and monitors.
    signals: mpsc::UnboundedSender<Info>,
    trap_exits: AtomicBool,
}

impl InstanceHandle {
    /// The instance's pid.
    #[must_use]
    pub fn pid(&self) -> Pid {
        self.shared.pid
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
        self.check_size(&msg)?;
        self.shared
            .mailbox
            .try_send(Message::Cast(msg))
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => {
                    self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                    CallError::Overloaded
                }
                mpsc::error::TrySendError::Closed(_) => CallError::Noproc,
            })
    }

    fn check_size(&self, msg: &Envelope) -> Result<(), CallError> {
        let len = u64::try_from(msg.body.len()).unwrap_or(u64::MAX);
        if len > self.shared.message_max_bytes {
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

    /// Stop now with reason `killed`, without `terminate`. A callback in
    /// progress is abandoned.
    pub fn kill(&self) {
        self.stop_now(ExitReason::Killed);
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
pub(crate) struct InstanceTask {
    pid: Pid,
    me: InstanceHandle,
    mailbox: mpsc::Receiver<Message>,
    signals: mpsc::UnboundedReceiver<Info>,
    self_mailbox: mpsc::WeakSender<Message>,
    dropped: Arc<AtomicU64>,
    control: watch::Receiver<Control>,
    state: watch::Sender<LifecycleState>,
    exit: watch::Sender<Option<ExitReason>>,
}

/// Create the channels for a new instance.
pub(crate) fn prepare(pid: Pid, spec: &InstanceSpec) -> (InstanceHandle, InstanceTask) {
    let (mailbox_tx, mailbox_rx) = mpsc::channel(spec.mailbox_max.max(1));
    let (control_tx, control_rx) = watch::channel(Control::Run);
    let (state_tx, state_rx) = watch::channel(LifecycleState::Starting);
    let (exit_tx, exit_rx) = watch::channel(None);
    let dropped = Arc::new(AtomicU64::new(0));
    let (signals_tx, signals_rx) = mpsc::unbounded_channel();
    let self_mailbox = mailbox_tx.downgrade();
    let handle = InstanceHandle {
        shared: Arc::new(Shared {
            pid,
            mailbox: mailbox_tx,
            control: control_tx,
            state: state_rx,
            exit: exit_rx,
            message_max_bytes: spec.message_max_bytes,
            dropped: dropped.clone(),
            signals: signals_tx,
            trap_exits: AtomicBool::new(false),
        }),
    };
    let task = InstanceTask {
        pid,
        me: handle.clone(),
        mailbox: mailbox_rx,
        signals: signals_rx,
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
        notify_exit(&rt, self.pid, &reason);
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
        let ctx = ProcessContext {
            rt,
            me: self.me.clone(),
            mailbox: self.self_mailbox.clone(),
            dropped: self.dropped.clone(),
        };
        let mut store = Store::new(&engine, InstanceState::new(self.pid, spec, ctx));
        store.limiter(|s| &mut s.limiter);
        store.epoch_deadline_callback(|mut ctx| {
            if ctx.data_mut().clock.expired() {
                Ok(UpdateDeadline::Interrupt)
            } else {
                // Cooperative preemption: give other instances the thread.
                Ok(UpdateDeadline::Yield(1))
            }
        });
        store.set_epoch_deadline(1);

        let instance = match self.start(&mut store, app, spec).await {
            Ok(instance) => instance,
            Err(reason) => return reason,
        };
        self.set_state(LifecycleState::Active);

        let stop = match self.serve(&mut store, &instance, spec).await {
            Ok(stop) => stop,
            Err(reason) => return reason,
        };

        self.set_state(LifecycleState::Stopping);
        self.mailbox.close();
        while self.mailbox.try_recv().is_ok() {}
        self.stop(&mut store, &instance, spec, stop).await
    }

    /// Instantiate and run `init` within the init timeout.
    async fn start(
        &mut self,
        store: &mut Store<InstanceState>,
        app: &LoadedApp,
        spec: &InstanceSpec,
    ) -> Result<App, ExitReason> {
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
        instance: &App,
        spec: &InstanceSpec,
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
                Some(info) = self.signals.recv() => Some(Message::Info(info)),
                message = self.mailbox.recv() => message,
            };
            let Some(message) = message else {
                return Ok(ExitReason::Shutdown);
            };

            store.data_mut().clock.start(spec.callback_deadline);
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
                        Ok(CallResult::Noreply) => Ok(()),
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
            if let Err(err) = outcome {
                tracing::debug!(error = %describe(&err), "callback failed");
                return Err(classify(&err, store.data()));
            }
        }
    }

    /// Run `terminate` within the shutdown timeout. Overrunning it is a kill.
    async fn stop(
        &mut self,
        store: &mut Store<InstanceState>,
        instance: &App,
        spec: &InstanceSpec,
        reason: ExitReason,
    ) -> ExitReason {
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

/// Tell linked instances and monitors that `pid` exited.
fn notify_exit(rt: &RuntimeInner, pid: Pid, reason: &ExitReason) {
    let notices = rt.links.on_exit(pid);
    for other in notices.linked {
        if let Some(handle) = rt.table.lookup(other) {
            handle.exit_signal(pid, reason.clone(), false);
        }
    }
    for (reference, watcher) in notices.monitors {
        if let Some(handle) = rt.table.lookup(watcher) {
            handle.signal(Info::Down {
                reference,
                pid,
                reason: reason.clone(),
            });
        }
    }
}

/// Map a failed guest call to an exit reason.
fn classify(err: &wasmtime::Error, state: &InstanceState) -> ExitReason {
    if state.limiter.memory_exceeded {
        return ExitReason::Crashed("memory_limit".to_string());
    }
    if state.limiter.table_exceeded {
        return ExitReason::Crashed("table_limit".to_string());
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
