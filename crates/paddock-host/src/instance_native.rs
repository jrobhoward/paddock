//! A native-process instance's lifecycle: the app's process,
//! through its [`Port`], in place of a Store.
//!
//! Same pid, mailbox, signals, operations, links, monitors and exit
//! reasons as a component's instance; one callback at a time. While a
//! callback runs the app may ask for process functions, answered with the
//! same host code a component's imports run (`process::Host` on the
//! instance's [`InstanceState`]). Differences:
//!
//! - A callback's deadline is wall time less the time spent answering the
//!   app's requests; past it the process is killed (`timeout`, or
//!   `crashed("init_timeout")` for init). The lease ending while a
//!   singleton's callback runs kills it too (`crashed("lease_ended")`).
//! - While idle the instance pings the app every `ping_interval`; no pong
//!   within `ping_timeout` kills it (`timeout`).
//! - The process ending is a crash with its status or signal, or
//!   `memory_limit` after its cgroup's OOM kill; a panic in the app is a
//!   crash with the panic's message.
//! - No snapshots, so no passivation, snapshot upgrades or moves.

use std::sync::Arc;
use std::time::Duration;

use paddock_native::protocol::{self as p, Answer, FromApp, Request, ToApp};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{AbortOnDrop, Control, InstanceTask, Message, Op, SnapshotError};
use crate::bindings::paddock::runtime::process;
use crate::failover::RunUntil;
use crate::health;
use crate::lifecycle::LifecycleState;
use crate::message::{Envelope, ExitReason, Info};
use crate::native::NativeCode;
use crate::native::convert;
use crate::native::port::{Incoming, Port};
use crate::output::{CapturedOutput, Tags};
use crate::probe;
use crate::runtime::RuntimeInner;
use crate::services::HostState;
use crate::services::logging::{self, LogRate, Logging};
use crate::spec::InstanceSpec;
use crate::state::{InstanceState, ProcessContext};
use crate::upgrade::UpgradeError;

/// A frame's name, for protocol errors.
fn frame_name(frame: &FromApp) -> &'static str {
    match frame {
        FromApp::Hello { .. } => "hello",
        FromApp::Init(_) => "init",
        FromApp::Reply(_) => "reply",
        FromApp::Done => "done",
        FromApp::Failed(_) => "failed",
        FromApp::Request { .. } => "request",
        FromApp::Log { .. } => "log",
        FromApp::Pong { .. } => "pong",
    }
}

/// The app broke the protocol: kill it and crash.
fn broke(port: &mut Port, what: String) -> ExitReason {
    port.kill();
    ExitReason::Crashed(format!("protocol: {what}"))
}

/// When the lease ends, for a callback's deadline; `None` without one.
fn lease_end(lease: Option<&watch::Receiver<RunUntil>>) -> Option<Instant> {
    match *lease?.borrow() {
        RunUntil::Always => None,
        RunUntil::Never => Some(Instant::now()),
        RunUntil::Until(end) => Some(end),
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn lease_changed(lease: Option<&mut watch::Receiver<RunUntil>>) {
    let Some(lease) = lease else {
        return std::future::pending().await;
    };
    if lease.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Log a record from the app.
async fn log(state: &mut InstanceState, level: p::Level, message: String) {
    logging::record(&mut HostState::<Logging>::service(state), level, message).await;
}

/// Answer a request with the process functions a component's imports run.
/// An error (over a host-object limit, a call to itself) crashes the
/// instance with its message, as a trap does.
async fn answer(state: &mut InstanceState, request: Request) -> Result<Answer, String> {
    use process::Host;
    let e = |e: wasmtime::Error| e.to_string();
    Ok(match request {
        Request::SelfPid => Answer::Pid(convert::from_wit_pid(state.self_().await.map_err(e)?)),
        Request::Cast { to, msg } => {
            Host::cast(state, convert::wit_pid(to), convert::wit_envelope(msg))
                .await
                .map_err(e)?;
            Answer::Unit
        }
        Request::Call {
            to,
            msg,
            timeout_ms,
        } => {
            let result = Host::call(
                state,
                convert::wit_pid(to),
                convert::wit_envelope(msg),
                timeout_ms,
            )
            .await
            .map_err(e)?;
            Answer::Called(
                result
                    .map(|env| convert::envelope(Envelope::from(env)))
                    .map_err(convert::call_error),
            )
        }
        Request::Reply { call_ref, msg } => {
            Host::reply(state, call_ref, convert::wit_envelope(msg))
                .await
                .map_err(e)?;
            Answer::Unit
        }
        Request::Link(other) => {
            Host::link(state, convert::wit_pid(other))
                .await
                .map_err(e)?;
            Answer::Unit
        }
        Request::Unlink(other) => {
            Host::unlink(state, convert::wit_pid(other))
                .await
                .map_err(e)?;
            Answer::Unit
        }
        Request::TrapExits(on) => {
            Host::trap_exits(state, on).await.map_err(e)?;
            Answer::Unit
        }
        Request::Monitor(other) => Answer::Ref(
            Host::monitor(state, convert::wit_pid(other))
                .await
                .map_err(e)?,
        ),
        Request::Demonitor(reference) => {
            Host::demonitor(state, reference).await.map_err(e)?;
            Answer::Unit
        }
        Request::Register(name) => Answer::Registered(
            Host::register(state, name)
                .await
                .map_err(e)?
                .map_err(convert::register_error),
        ),
        Request::Whereis(name) => Answer::MaybePid(
            Host::whereis(state, name)
                .await
                .map_err(e)?
                .map(convert::from_wit_pid),
        ),
        Request::SendAfter { ms, msg } => Answer::Ref(
            Host::send_after(state, ms, convert::wit_envelope(msg))
                .await
                .map_err(e)?,
        ),
        Request::CancelTimer(reference) => {
            Host::cancel_timer(state, reference).await.map_err(e)?;
            Answer::Unit
        }
        Request::Exit { to, reason } => {
            let reason = convert::host_exit_reason(reason);
            Host::exit(state, convert::wit_pid(to), (&reason).into())
                .await
                .map_err(e)?;
            Answer::Unit
        }
    })
}

impl InstanceTask {
    /// Run a native instance to its exit reason.
    pub(super) async fn native(
        &mut self,
        rt: Arc<RuntimeInner>,
        code: &NativeCode,
        spec: &InstanceSpec,
    ) -> ExitReason {
        let weak = Arc::downgrade(&rt);
        let ctx = ProcessContext {
            rt: rt.clone(),
            me: self.me.clone(),
            mailbox: self.self_mailbox.clone(),
            dropped: self.dropped.clone(),
        };
        let mut state = match InstanceState::new(self.pid, spec, ctx) {
            Ok(state) => state,
            Err(why) => return ExitReason::Crashed(why),
        };
        if let Some(stop) = self.await_lease(state.lease.clone()).await {
            return stop;
        }
        let tags = Arc::new(Tags {
            app: spec.app.clone(),
            version: spec.version.clone(),
            pid: self.pid,
        });
        let rate = LogRate::new(
            rt.services.logging.clone(),
            spec.services.logging.rate_limit,
        );
        let output = CapturedOutput::new(tags, "stderr", rate);
        let name = format!("{}-{}-{}", self.pid.node, self.pid.id, self.pid.incarnation);
        let mut port = match Port::spawn(rt.native_config(), code, spec, &name, output) {
            Ok(port) => port,
            Err(why) => return ExitReason::Crashed(why),
        };
        let reason = self
            .native_run(&mut state, &mut port, spec, weak, &rt)
            .await;
        port.finish().await;
        reason
    }

    async fn native_run(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &InstanceSpec,
        weak: std::sync::Weak<RuntimeInner>,
        rt: &RuntimeInner,
    ) -> ExitReason {
        let started = Instant::now();
        if let Err(reason) = self.native_hello(port, spec.init_timeout).await {
            return reason;
        }
        port.send(&ToApp::Init {
            args: spec.init_args.clone(),
        });
        let left = spec.init_timeout.saturating_sub(started.elapsed());
        let init_timeout = ExitReason::Crashed("init_timeout".into());
        match self.native_callback(state, port, left, &init_timeout).await {
            Ok(FromApp::Init(Ok(()))) => {}
            Ok(FromApp::Init(Err(msg))) => {
                return ExitReason::Crashed(format!("init failed: {msg}"));
            }
            Ok(other) => return broke(port, format!("expected init, got {}", frame_name(&other))),
            Err(reason) => return reason,
        }
        self.set_state(LifecycleState::Active);
        self.native_announce(state);
        let _health = spec
            .health
            .map(|check| AbortOnDrop(tokio::spawn(health::watch(weak, self.me.clone(), check))));
        let native = rt.native_config();
        let (interval, timeout) = (native.ping_interval, native.ping_timeout);
        let stop = match self
            .native_serve(state, port, spec, interval, timeout)
            .await
        {
            Ok(stop) => stop,
            Err(reason) => return reason,
        };
        self.set_state(LifecycleState::Stopping);
        self.mailbox.close();
        while self.mailbox.try_recv().is_ok() {}
        self.native_stop(state, port, spec, stop).await
    }

    /// Wait for the app's hello within the init timeout.
    async fn native_hello(&mut self, port: &mut Port, timeout: Duration) -> Result<(), ExitReason> {
        let hello = tokio::time::timeout(timeout, port.recv());
        let crashed = |why: String| Err(ExitReason::Crashed(why));
        match super::killable(&mut self.control, hello).await? {
            Err(_elapsed) => {
                port.kill();
                crashed("init_timeout".into())
            }
            Ok(Some(Incoming::Frame(FromApp::Hello { version }))) if version == p::VERSION => {
                Ok(())
            }
            Ok(Some(Incoming::Frame(FromApp::Hello { version }))) => {
                port.kill();
                crashed(format!(
                    "native protocol version {version}; this node speaks {}",
                    p::VERSION
                ))
            }
            Ok(Some(Incoming::Frame(other))) => Err(broke(
                port,
                format!("expected hello, got {}", frame_name(&other)),
            )),
            Ok(Some(Incoming::Broken(e))) => Err(broke(port, e)),
            Ok(None) => Err(port.exit_reason().await),
        }
    }

    /// Run one callback the port has been sent: answer the app's requests
    /// and record its logs until its result. `budget` is the callback's
    /// time, not counting time spent answering requests; past it the
    /// process is killed with `timeout`.
    async fn native_callback(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        budget: Duration,
        timeout: &ExitReason,
    ) -> Result<FromApp, ExitReason> {
        let start = Instant::now();
        let mut host = Duration::ZERO;
        let mut lease = state.lease.clone();
        loop {
            let deadline = start + budget + host;
            let lease_end = lease_end(lease.as_ref());
            let next = tokio::select! {
                biased;
                killed = self.control.wait_for(|c| matches!(c, Control::Kill(_))) => {
                    port.kill();
                    return Err(match killed.as_deref() {
                        Ok(Control::Kill(reason)) => reason.clone(),
                        _ => ExitReason::Killed,
                    });
                }
                () = tokio::time::sleep_until(deadline) => {
                    port.kill();
                    return Err(timeout.clone());
                }
                () = sleep_until(lease_end) => {
                    state.lease_ended = true;
                    port.kill();
                    return Err(ExitReason::Crashed("lease_ended".into()));
                }
                () = lease_changed(lease.as_mut()) => continue,
                incoming = port.recv() => incoming,
            };
            match next {
                Some(Incoming::Frame(FromApp::Request { id, request })) => {
                    let asked = Instant::now();
                    match super::killable(&mut self.control, answer(state, request)).await {
                        Ok(Ok(answer)) => port.send(&ToApp::Answer { id, answer }),
                        Ok(Err(why)) => {
                            port.kill();
                            return Err(ExitReason::Crashed(why));
                        }
                        Err(killed) => {
                            port.kill();
                            return Err(killed);
                        }
                    }
                    host += asked.elapsed();
                }
                Some(Incoming::Frame(FromApp::Log { level, message })) => {
                    log(state, level, message).await;
                }
                Some(Incoming::Frame(FromApp::Pong { .. })) => {}
                Some(Incoming::Frame(FromApp::Failed(why))) => {
                    return Err(ExitReason::Crashed(why));
                }
                Some(Incoming::Frame(FromApp::Hello { .. })) => {
                    return Err(broke(port, "hello during a callback".into()));
                }
                Some(Incoming::Frame(result)) => return Ok(result),
                Some(Incoming::Broken(e)) => return Err(broke(port, e)),
                None => return Err(port.exit_reason().await),
            }
        }
    }

    /// Handle messages until a stop is requested (`Ok` with its reason) or
    /// the instance fails (`Err`).
    async fn native_serve(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &InstanceSpec,
        ping_interval: Duration,
        ping_timeout: Duration,
    ) -> Result<ExitReason, ExitReason> {
        // The ping in flight: its id and when its pong is due.
        let mut ping: Option<(u64, Instant)> = None;
        let mut next_ping = 0u64;
        let mut idle_since = Instant::now();
        loop {
            match self.control.borrow_and_update().clone() {
                Control::Run => {}
                Control::Shutdown(reason) => return Ok(reason),
                Control::Kill(reason) => {
                    port.kill();
                    return Err(reason);
                }
            }
            let ping_at = ping.map_or(idle_since + ping_interval, |(_, due)| due);
            let message = tokio::select! {
                biased;
                changed = self.control.changed() => {
                    if changed.is_err() {
                        port.kill();
                        return Err(ExitReason::Killed);
                    }
                    continue;
                }
                Some(op) = self.ops.recv() => {
                    self.native_op(state, *op);
                    continue;
                }
                Some(info) = self.signals.recv() => match info.resolve() {
                    Some(info) => Message::Info(info),
                    None => continue,
                },
                message = self.mailbox.recv() => match message {
                    Some(message) => message,
                    None => return Ok(ExitReason::Shutdown),
                },
                incoming = port.recv() => {
                    match incoming {
                        Some(Incoming::Frame(FromApp::Log { level, message })) => {
                            log(state, level, message).await;
                        }
                        Some(Incoming::Frame(FromApp::Pong { id })) => {
                            if ping.is_some_and(|(want, _)| want == id) {
                                ping = None;
                                idle_since = Instant::now();
                            }
                        }
                        Some(Incoming::Frame(other)) => {
                            return Err(broke(port, format!("{} between callbacks", frame_name(&other))));
                        }
                        Some(Incoming::Broken(e)) => return Err(broke(port, e)),
                        None => return Err(port.exit_reason().await),
                    }
                    continue;
                }
                () = tokio::time::sleep_until(ping_at) => {
                    if ping.is_some() {
                        tracing::warn!("native instance missed a ping");
                        port.kill();
                        return Err(ExitReason::Timeout);
                    }
                    next_ping += 1;
                    port.send(&ToApp::Ping { id: next_ping });
                    ping = Some((next_ping, Instant::now() + ping_timeout));
                    continue;
                }
            };
            // Out of lease, the message waits here; a stop drops it.
            if let Some(stop) = self.await_lease(state.lease.clone()).await {
                return match *self.control.borrow() {
                    Control::Shutdown(_) => Ok(stop),
                    Control::Run | Control::Kill(_) => {
                        port.kill();
                        Err(stop)
                    }
                };
            }
            self.native_handle(state, port, spec, message).await?;
            ping = None;
            idle_since = Instant::now();
        }
    }

    /// Run one message's callback.
    async fn native_handle(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &InstanceSpec,
        message: Message,
    ) -> Result<(), ExitReason> {
        let kind = match &message {
            Message::Call { .. } => "call",
            Message::Cast(_) => "cast",
            Message::Info(_) => "info",
        };
        probe::callback_start(self.pid, kind);
        let started = Instant::now();
        let deadline = spec.callback_deadline;
        let outcome = match message {
            Message::Call { msg, reply } => {
                let call_ref = self.next_ref;
                self.next_ref += 1;
                state.await_reply(call_ref, reply);
                port.send(&ToApp::Call {
                    call_ref,
                    msg: convert::envelope(msg),
                });
                match self
                    .native_callback(state, port, deadline, &ExitReason::Timeout)
                    .await
                {
                    Ok(FromApp::Reply(Some(env))) => {
                        // Already answered with `reply`, or the caller gave
                        // up: dropped.
                        if let Some(waiting) = state.pending.remove(&call_ref) {
                            let _ = waiting.send(convert::host_envelope(env));
                        }
                        Ok(())
                    }
                    Ok(FromApp::Reply(None)) => state.check_pending().map_err(|e| {
                        port.kill();
                        ExitReason::Crashed(e.to_string())
                    }),
                    Ok(other) => Err(broke(
                        port,
                        format!("expected reply, got {}", frame_name(&other)),
                    )),
                    Err(reason) => Err(reason),
                }
            }
            Message::Cast(msg) => {
                port.send(&ToApp::Cast {
                    msg: convert::envelope(msg),
                });
                self.native_done(state, port, deadline).await
            }
            Message::Info(info) => {
                if let Info::Timer { reference, .. } = &info {
                    state.timers.fired(*reference);
                }
                match convert::info(info) {
                    Some(info) => {
                        port.send(&ToApp::Info { info });
                        self.native_done(state, port, deadline).await
                    }
                    None => Ok(()),
                }
            }
        };
        let took = started.elapsed();
        self.stats.callback(kind, took);
        if probe::callback_done_enabled() {
            let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
            probe::callback_done(self.pid, kind, ns, outcome.is_ok());
        }
        outcome
    }

    /// Run a callback answered with `done`.
    async fn native_done(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        deadline: Duration,
    ) -> Result<(), ExitReason> {
        match self
            .native_callback(state, port, deadline, &ExitReason::Timeout)
            .await?
        {
            FromApp::Done => Ok(()),
            other => Err(broke(
                port,
                format!("expected done, got {}", frame_name(&other)),
            )),
        }
    }

    /// Run `terminate` within the shutdown timeout; a stop while out of
    /// lease kills instead, as `terminate` is app code.
    async fn native_stop(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &InstanceSpec,
        reason: ExitReason,
    ) -> ExitReason {
        if !state.within_lease() {
            tracing::info!(%reason, "singleton out of lease: terminate skipped");
            port.kill();
            return ExitReason::Killed;
        }
        port.send(&ToApp::Terminate {
            reason: convert::exit_reason(&reason),
        });
        match self
            .native_callback(state, port, spec.shutdown_timeout, &ExitReason::Killed)
            .await
        {
            Ok(FromApp::Done) => reason,
            Ok(other) => broke(port, format!("expected done, got {}", frame_name(&other))),
            Err(reason) => reason,
        }
    }

    /// An operation between callbacks: a native instance takes no
    /// snapshot, so only announcements are done.
    fn native_op(&self, state: &InstanceState, op: Op) {
        match op {
            Op::Upgrade { reply, .. } => {
                let _ = reply.send(Err(UpgradeError::Invalid(
                    "a native-process app upgrades by restart or blue-green".into(),
                )));
            }
            Op::Snapshot { reply } | Op::MoveOut { reply } => {
                let _ = reply.send(Err(SnapshotError::NotSupported));
            }
            Op::Announce => self.native_announce(state),
        }
    }

    /// Queue the services' host events, ahead of mailbox messages.
    fn native_announce(&self, state: &InstanceState) {
        for (name, msg) in state.announcements() {
            self.me.signal(Info::HostEvent { name, msg });
        }
    }
}
