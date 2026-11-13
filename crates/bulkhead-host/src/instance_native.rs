//! A native-process instance's lifecycle: the app's process, through its
//! [`Port`].
//!
//! The pid, mailbox, signals, operations, links, monitors and exit
//! reasons are the instance's (`instance`); one callback at a time. While
//! a callback runs the app may ask for process functions, answered by the
//! host's process functions (`process::Host` on the instance's
//! [`InstanceState`]). How the process is run:
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
//! - Snapshots are frames: `SaveSnapshot` between callbacks, and
//!   `LoadSnapshot` in place of `Init` in a new process, for a snapshot
//!   upgrade (the new version's process beside the old until it has
//!   loaded) and a singleton moved here. A rustbridge plugin takes none.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_native::hot::KvReply;
use bulkhead_native::protocol::{self as p, Answer, FromApp, Request, ToApp};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{AbortOnDrop, Control, InstanceTask, Message, Op, SnapshotError};
use crate::failover::RunUntil;
use crate::health;
use crate::lifecycle::LifecycleState;
use crate::listener::Accepted;
use crate::message::{CallError, ExitReason, Info};
use crate::native::Passed;
use crate::native::Program;
use crate::native::convert;
use crate::native::port::{Incoming, KvCall, Port};
use crate::output::{CapturedOutput, Tags};
use crate::probe;
use crate::runtime::{LoadedApp, RuntimeInner};
use crate::services::config::Config;
use crate::services::egress::Egress;
use crate::services::groups::Groups;
use crate::services::keyvalue::publish::Publications;
use crate::services::keyvalue::storage::Outcome;
use crate::services::keyvalue::{KeyValue, WATCH_EVENT};
use crate::services::logging::{self, LogRate, Logging};
use crate::services::metrics::MetricsService;
use crate::services::secrets::Secrets;
#[cfg(unix)]
use crate::services::sockets::{NativeReply, Sockets};
use crate::services::sql::Sql;
use crate::services::{HostState, NativeCall, NativeCallError};
use crate::spec::InstanceSpec;
use crate::state::{InstanceState, ProcessContext};
use crate::upgrade::UpgradeError;

/// A frame's name, for protocol errors.
fn frame_name(frame: &FromApp) -> &'static str {
    match frame {
        FromApp::Hello { .. } => "hello",
        FromApp::Init(_) => "init",
        FromApp::Reply(_) => "reply",
        FromApp::Snapshot(_) => "snapshot",
        FromApp::Done => "done",
        FromApp::Failed(_) => "failed",
        FromApp::Request { .. } => "request",
        FromApp::Log { .. } => "log",
        FromApp::Pong { .. } => "pong",
        FromApp::Metrics(_) => "metrics",
    }
}

/// What woke a callback's wait.
enum Step {
    Timer,
    LeaseEnded,
    LeaseChanged,
    Incoming(Option<Incoming>),
}

/// An idle native instance's pings: one in flight at a time, sent
/// `interval` after the instance last did something and answered within
/// `timeout`.
struct Pings {
    /// The ping in flight: its id and when its pong is due.
    in_flight: Option<(u64, Instant)>,
    sent: u64,
    idle_since: Instant,
    interval: Duration,
    timeout: Duration,
}

impl Pings {
    /// The app answered, or handled something: no ping is due before
    /// another interval.
    fn answered(&mut self) {
        self.in_flight = None;
        self.idle_since = Instant::now();
    }

    /// `timer` fired: send a ping if one is due, or kill the app if the
    /// one in flight went unanswered (`timeout`); otherwise move `timer`.
    fn due(
        &mut self,
        port: &mut Port,
        timer: std::pin::Pin<&mut tokio::time::Sleep>,
    ) -> Result<(), ExitReason> {
        let at = self
            .in_flight
            .map_or(self.idle_since + self.interval, |(_, due)| due);
        if Instant::now() < at {
            timer.reset(at);
            return Ok(());
        }
        if self.in_flight.is_some() {
            tracing::warn!("native instance missed a ping");
            port.kill();
            return Err(ExitReason::Timeout);
        }
        self.sent += 1;
        port.send(&ToApp::Ping { id: self.sent });
        let due = Instant::now() + self.timeout;
        self.in_flight = Some((self.sent, due));
        timer.reset(due);
        Ok(())
    }
}

/// The lease timer fired: a lease that has ended kills the app
/// (`crashed: lease_ended`); otherwise the timer moves to its end.
fn lease_due(
    port: &mut Port,
    lease: Option<&watch::Receiver<RunUntil>>,
    timer: std::pin::Pin<&mut tokio::time::Sleep>,
) -> Result<(), ExitReason> {
    match lease_end(lease) {
        Some(end) if end <= Instant::now() => Err(lease_ended(port)),
        end => {
            timer.reset(end.unwrap_or_else(far_off));
            Ok(())
        }
    }
}

/// The lease has ended: kill the app (`crashed: lease_ended`), unless the
/// runner's watchdog already did while this node did not run (a stopped
/// node), whose exit then says so (`crashed: killed_at_lease_end`).
fn lease_ended(port: &mut Port) -> ExitReason {
    port.exited_reason().unwrap_or_else(|| {
        port.kill();
        ExitReason::Crashed("lease_ended".into())
    })
}

/// A time no timer reaches.
fn far_off() -> Instant {
    Instant::now() + Duration::from_secs(365 * 86_400)
}

/// What woke a native instance between callbacks: a message to handle,
/// or an operation.
enum Woke {
    Next(Message),
    Op(Box<Op>),
}

/// A frame from the app between callbacks: a log, or the pong for the
/// ping in flight, which `pings` then counts answered. Anything else
/// breaks the protocol.
async fn between_callbacks(
    state: &mut InstanceState,
    port: &mut Port,
    incoming: Option<Incoming>,
    pings: &mut Pings,
) -> Result<(), ExitReason> {
    match incoming {
        Some(Incoming::Frame(FromApp::Log { level, message })) => {
            log(state, level, &message);
            Ok(())
        }
        Some(Incoming::Frame(FromApp::Pong { id })) => {
            if pings.in_flight.is_some_and(|(want, _)| want == id) {
                pings.answered();
            }
            Ok(())
        }
        Some(Incoming::Frame(other)) => Err(broke(
            port,
            format!("{} between callbacks", frame_name(&other)),
        )),
        Some(Incoming::Kv(_)) => Err(broke(port, "a request between callbacks".into())),
        Some(Incoming::Broken(e)) => Err(broke(port, e)),
        None => Err(port.exit_reason().await),
    }
}

/// A hot frame's keyvalue request (ROADMAP T7).
async fn kv_hot(state: &mut InstanceState, call: KvCall) -> Result<Outcome, p::KvError> {
    HostState::<KeyValue>::service(state)
        .native_hot(call.bucket, call.op, call.key, call.value)
        .await
}

/// Answer a hot keyvalue request with what the store returned.
fn send_kv(port: &mut Port, id: u64, outcome: Outcome) {
    let reply = match &outcome {
        Outcome::Value(v) => KvReply::Value(v.as_deref()),
        Outcome::Exists(e) => KvReply::Exists(*e),
        Outcome::Done => KvReply::Unit,
        other => {
            return port.send(&ToApp::Answer {
                id,
                answer: Answer::Kv(Err(p::KvError::Other(format!(
                    "unexpected storage outcome {other:?}"
                )))),
            });
        }
    };
    port.send_kv(id, &reply);
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

/// A new snapshot of some published bucket; never without publications.
async fn published_changed(publications: Option<&mut Publications>) {
    let Some(p) = publications else {
        return std::future::pending().await;
    };
    if p.changed.changed().await.is_err() {
        std::future::pending::<()>().await;
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
fn log(state: &mut InstanceState, level: p::Level, message: &str) {
    logging::record(&mut HostState::<Logging>::service(state), level, message);
}

/// A cast's answer: `Unit` once queued, or why it was not.
fn cast_answer(sent: Result<(), CallError>) -> Answer {
    match sent {
        Ok(()) => Answer::Unit,
        Err(err) => Answer::CastFailed(convert::cast_error(&err)),
    }
}

/// Answer a request with the host's process functions. An error (over a
/// host-object limit, a call to itself) crashes the instance with its
/// message. `deadline` is the callback's
/// deadline, which caps how long a cast waits for room.
async fn answer(
    state: &mut InstanceState,
    request: Request,
    deadline: Duration,
) -> Result<Answer, String> {
    Ok(match request {
        Request::SelfPid => Answer::Pid(convert::pid(state.self_pid())),
        Request::Cast { to, msg } => {
            cast_answer(state.try_cast(convert::host_pid(to), convert::host_envelope(msg)))
        }
        Request::CastWait {
            to,
            msg,
            timeout_ms,
        } => {
            let timeout = Duration::from_millis(timeout_ms.into()).min(deadline);
            cast_answer(
                state
                    .cast_wait(convert::host_pid(to), convert::host_envelope(msg), timeout)
                    .await,
            )
        }
        Request::Call {
            to,
            msg,
            timeout_ms,
        } => {
            let result = state
                .call_to(
                    convert::host_pid(to),
                    convert::host_envelope(msg),
                    Duration::from_millis(timeout_ms.into()),
                )
                .await?;
            Answer::Called(
                result
                    .map(convert::envelope)
                    .map_err(|e| convert::call_error(&e)),
            )
        }
        Request::CallApp {
            app,
            msg,
            timeout_ms,
        } => {
            let result = state
                .call_app(
                    &app,
                    convert::host_envelope(msg),
                    Duration::from_millis(timeout_ms.into()),
                )
                .await?;
            Answer::Called(
                result
                    .map(convert::envelope)
                    .map_err(|e| convert::call_error(&e)),
            )
        }
        Request::Reply { call_ref, msg } => {
            state.reply_to(call_ref, convert::host_envelope(msg));
            Answer::Unit
        }
        Request::Link(other) => {
            state.link_to(convert::host_pid(other))?;
            Answer::Unit
        }
        Request::Unlink(other) => {
            state.unlink_from(convert::host_pid(other));
            Answer::Unit
        }
        Request::TrapExits(on) => {
            state.set_trapping_exits(on);
            Answer::Unit
        }
        Request::Monitor(other) => Answer::Ref(state.monitor_pid(convert::host_pid(other))?),
        Request::Demonitor(reference) => {
            state.demonitor_ref(reference);
            Answer::Unit
        }
        Request::Register(name) => {
            Answer::Registered(state.register_name(&name).map_err(convert::register_error))
        }
        Request::Whereis(name) => Answer::MaybePid(state.whereis_name(&name).map(convert::pid)),
        Request::SendAfter { ms, msg } => {
            Answer::Ref(state.send_after_ms(ms, convert::host_envelope(msg))?)
        }
        Request::CancelTimer(reference) => {
            state.cancel_timer_ref(reference);
            Answer::Unit
        }
        Request::Exit { to, reason } => {
            state.exit_to(convert::host_pid(to), convert::host_exit_reason(reason));
            Answer::Unit
        }
        services => service_answer(state, services).await?,
    })
}

/// A socket request's answer, after passing the stream it made, if any,
/// on the descriptor socket.
#[cfg(unix)]
fn pass_socket(
    port: &mut Port,
    reply: Result<NativeReply, p::NetError>,
) -> Result<p::NetAnswer, p::NetError> {
    use std::os::fd::AsFd;
    let (mut answer, fd) = reply?;
    if let Some(fd) = fd {
        let passed = port
            .send_fd(fd.as_fd(), Passed::Stream)
            .map_err(p::NetError::Other)?;
        match &mut answer {
            p::NetAnswer::Stream(s) | p::NetAnswer::Accepted(Some(s)) => s.fd = passed,
            _ => {}
        }
    }
    Ok(answer)
}

/// Answer a request for a host service the app may declare (keyvalue,
/// groups, config, secrets, metrics, SQL), on each service's checks. [`answer`] handles the process functions first, so any
/// other request here is an error.
async fn service_answer(state: &mut InstanceState, request: Request) -> Result<Answer, String> {
    Ok(match request {
        Request::Kv(request) => {
            Answer::Kv(HostState::<KeyValue>::service(state).native(request).await)
        }
        Request::KvOpen(bucket) => {
            Answer::KvOpened(HostState::<KeyValue>::service(state).native_open(&bucket))
        }
        Request::Groups(request) => {
            Answer::Groups(HostState::<Groups>::service(state).state.native(request))
        }
        Request::ConfigGet(key) => Answer::Config(
            HostState::<Config>::service(state)
                .native(Some(&key))
                .map(|found| found.into_iter().next().map(|(_, v)| v)),
        ),
        Request::ConfigAll => Answer::ConfigAll(HostState::<Config>::service(state).native(None)),
        Request::Secret(name) => Answer::Secret(
            HostState::<Secrets>::service(state)
                .state
                .get_secret(&name)
                .await
                .map(p::Bytes),
        ),
        Request::MetricRegister {
            kind,
            name,
            help,
            labels,
            buckets,
        } => Answer::Metric(
            HostState::<MetricsService>::service(state)
                .state
                .native_register(kind, &name, &help, &labels, &buckets),
        ),
        Request::Sql(request) => {
            Answer::Sql(HostState::<Sql>::service(state).native(request).await)
        }
        Request::Http(request) => Answer::Http(
            HostState::<Egress>::service(state)
                .state
                .native(request)
                .await,
        ),
        Request::Extension {
            package,
            function,
            args,
        } => Answer::Extension(extension_call(state, &package, function, args.0).await?),
        other => return Err(format!("not a service request: {other:?}")),
    })
}

/// Call `function` of the extension serving `package` (ROADMAP Z8h). An
/// extension's crash is the instance's.
async fn extension_call(
    state: &mut InstanceState,
    package: &str,
    function: String,
    args: Vec<u8>,
) -> Result<Result<p::Bytes, p::ExtensionError>, String> {
    let Some(ext) = state
        .ctx
        .rt
        .extensions
        .iter()
        .find(|e| e.package() == package)
        .cloned()
    else {
        return Ok(Err(p::ExtensionError::NoSuchExtension));
    };
    match ext.call(state, NativeCall::new(function, args)).await {
        Ok(result) => Ok(Ok(p::Bytes(result))),
        Err(NativeCallError::NoSuchFunction(name)) => {
            Ok(Err(p::ExtensionError::NoSuchFunction(name)))
        }
        Err(NativeCallError::BadArguments(why)) => Ok(Err(p::ExtensionError::BadArguments(why))),
        Err(NativeCallError::Crash(why)) => Err(format!("extension {package}: {why}")),
    }
}

impl InstanceTask {
    /// Run a native instance to its exit reason.
    pub(super) async fn native(
        &mut self,
        rt: Arc<RuntimeInner>,
        code: &Program,
        spec: &InstanceSpec,
    ) -> ExitReason {
        let weak = Arc::downgrade(&rt);
        let ctx = ProcessContext {
            rt: rt.clone(),
            me: self.me.clone(),
            mailbox: self.self_mailbox.clone(),
            dropped: self.dropped.clone(),
            stats: self.stats.clone(),
        };
        let mut state = match InstanceState::new(self.pid, spec, ctx) {
            Ok(state) => state,
            Err(why) => return ExitReason::Crashed(why),
        };
        self.native_published = HostState::<KeyValue>::service(&mut state).publications();
        if let Some(stop) = self.await_lease(state.lease.clone()).await {
            return stop;
        }
        let mut port = match self.native_port(&rt, code, spec).await {
            Ok(port) => port,
            Err(why) => return ExitReason::Crashed(why),
        };
        self.me.set_native_memory(port.memory_source());
        let reason = self
            .native_run(&mut state, &mut port, spec, weak, &rt)
            .await;
        self.me.set_native_memory(None);
        port.finish().await;
        reason
    }

    /// Start `code`'s process for `spec`, its stderr logged as the
    /// instance's. Each process the instance starts gets a cgroup name of
    /// its own: an upgrade starts the new one beside the old.
    async fn native_port(
        &mut self,
        rt: &Arc<RuntimeInner>,
        code: &Program,
        spec: &InstanceSpec,
    ) -> Result<Port, String> {
        let tags = Arc::new(Tags {
            app: spec.app.clone(),
            version: spec.version.clone(),
            pid: self.pid,
        });
        let rate = LogRate::new(
            rt.services.logging.clone(),
            spec.services.logging.rate_limit,
        );
        let output = CapturedOutput::new(tags.clone(), "stderr", rate);
        self.native_spawns += 1;
        let mut name = format!("{}-{}-{}", self.pid.node, self.pid.id, self.pid.incarnation);
        if self.native_spawns > 1 {
            name = format!("{name}-{}", self.native_spawns);
        }
        // An instance bound to a lease is killed at its end by the
        // runner's watchdog too, which runs while the node does not.
        let lease = spec.bound_to_lease.then(|| rt.failover.run_until());
        #[cfg_attr(
            not(any(target_os = "linux", target_os = "freebsd")),
            allow(unused_mut)
        )]
        let mut port = Port::spawn(rt.native_config(), code, spec, &name, output, lease).await?;
        // The app's threads may open sender channels (ROADMAP AP1), which
        // end with this process.
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        port.attach_senders(crate::native::sender::SenderContext {
            rt: rt.clone(),
            me: self.me.clone(),
            tags,
            stats: self.stats.clone(),
            spec: spec.clone(),
            host_objects: rt.host_objects().grant(&spec.host_objects)?,
            spin: rt.native_config().spin,
        });
        Ok(port)
    }

    async fn native_run(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &InstanceSpec,
        weak: std::sync::Weak<RuntimeInner>,
        rt: &RuntimeInner,
    ) -> ExitReason {
        // The first instance of a singleton moved here with its state
        // starts from the moved snapshot (`crate::handover`).
        let handover = rt.handovers.take(&spec.app);
        if let Some(h) = &handover {
            tracing::info!(
                from_version = %h.version,
                bytes = h.snapshot.len(),
                "starting from a moved snapshot"
            );
        }
        let from = handover
            .as_ref()
            .map(|h| (h.version.as_str(), h.snapshot.as_slice()));
        match self.native_start(state, port, spec, from).await {
            Ok(Ok(())) => {}
            Ok(Err(msg)) if handover.is_some() => {
                return ExitReason::Crashed(format!("handover: load-snapshot refused: {msg}"));
            }
            Ok(Err(msg)) => return ExitReason::Crashed(format!("init failed: {msg}")),
            Err(reason) => return reason,
        }
        self.set_state(LifecycleState::Active);
        self.native_announce(state);
        let _health = spec
            .health
            .map(|check| AbortOnDrop(tokio::spawn(health::watch(weak, self.me.clone(), check))));
        let native = rt.native_config();
        let (interval, timeout) = (native.ping_interval, native.ping_timeout);
        let mut spec = spec.clone();
        let stop = match self
            .native_serve(state, port, &mut spec, interval, timeout)
            .await
        {
            Ok(stop) => stop,
            Err(reason) => return reason,
        };
        self.set_state(LifecycleState::Stopping);
        self.mailbox.close();
        while self.mailbox.try_recv().is_ok() {}
        self.native_stop(state, port, &spec, stop).await
    }

    /// Start the app in `port`: its hello, then `Init`, or `LoadSnapshot`
    /// with `from` (the version that wrote the snapshot, and the
    /// snapshot), within the init timeout. `Ok(Err)` is the app's init
    /// error or refusal; `Err` ends the process (`crashed: init_timeout`
    /// past the timeout).
    async fn native_start(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &InstanceSpec,
        from: Option<(&str, &[u8])>,
    ) -> Result<Result<(), String>, ExitReason> {
        let started = Instant::now();
        self.native_hello(port, spec.init_timeout).await?;
        // A new process: every published bucket's latest snapshot, before
        // `init` can read one.
        self.native_publish(port, true);
        port.send(&match from {
            Some((from_version, snapshot)) => ToApp::LoadSnapshot {
                from_version: from_version.to_string(),
                snapshot: snapshot.to_vec(),
            },
            None => ToApp::Init {
                args: spec.init_args.clone(),
            },
        });
        let left = spec.init_timeout.saturating_sub(started.elapsed());
        let init_timeout = ExitReason::Crashed("init_timeout".into());
        match self
            .native_callback(state, port, left, &init_timeout)
            .await?
        {
            FromApp::Init(result) => Ok(result),
            other => Err(broke(
                port,
                format!("expected init, got {}", frame_name(&other)),
            )),
        }
    }

    /// An operation between callbacks, in `native_serve`.
    async fn native_serve_op(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &mut InstanceSpec,
        op: Op,
        pings: &mut Pings,
    ) -> Result<(), ExitReason> {
        // `Some` is the reason the instance cannot continue.
        self.native_op(state, port, spec, op)
            .await
            .map_or(Ok(()), Err)?;
        // The app answered, or was not asked: either way a ping in flight
        // is moot.
        pings.answered();
        Ok(())
    }

    /// Pass the running process each published bucket's latest snapshot
    /// it has not had (`fresh`: a new process, which has had none). A
    /// snapshot that cannot be passed leaves the app reading the bucket
    /// from the node.
    fn native_publish(&mut self, port: &mut Port, fresh: bool) {
        let Some(publications) = self.native_published.as_mut() else {
            return;
        };
        for (bucket, versions, sent) in &mut publications.buckets {
            if fresh {
                *sent = None;
            }
            let Some(version) = versions.borrow().clone() else {
                continue;
            };
            if *sent == Some(version.revision) {
                continue;
            }
            match port.send_published(&version) {
                Ok(fd) => {
                    port.send(&ToApp::Published {
                        bucket: bucket.clone(),
                        revision: version.revision,
                        fd,
                    });
                    *sent = Some(version.revision);
                }
                Err(e) => tracing::warn!(%bucket, error = %e, "published snapshot not passed"),
            }
        }
    }

    /// `SaveSnapshot` between callbacks, within the callback deadline. A
    /// failure ends the process: the app crashed, or ran out of time.
    async fn native_save(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &InstanceSpec,
    ) -> Result<Vec<u8>, ExitReason> {
        port.send(&ToApp::SaveSnapshot);
        match self
            .native_callback(state, port, spec.callback_deadline, &ExitReason::Timeout)
            .await?
        {
            FromApp::Snapshot(bytes) => Ok(bytes.0),
            other => Err(broke(
                port,
                format!("expected snapshot, got {}", frame_name(&other)),
            )),
        }
    }

    /// Snapshot upgrade: `SaveSnapshot` in the running process, then the
    /// new version's process started with `LoadSnapshot`. The running
    /// process is untouched until the new one has loaded, so a failure
    /// after the snapshot keeps the old version running. A kill during
    /// the upgrade returns `SaveFailed` with the kill's reason, which ends
    /// the instance.
    async fn native_upgrade(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &mut InstanceSpec,
        app: &LoadedApp,
        next: InstanceSpec,
        from_version: &str,
    ) -> Result<Duration, UpgradeError> {
        let next = state.ctx.rt.with_driver(next);
        let started = std::time::Instant::now();
        let code = app.native();
        if code.kind() != NativeProgram::Executable {
            return Err(UpgradeError::Invalid(
                "a native executable upgrades in snapshot mode to a native executable only".into(),
            ));
        }
        self.set_state(LifecycleState::Upgrading);
        let snapshot = self
            .native_save(state, port, spec)
            .await
            .map_err(UpgradeError::SaveFailed)?;
        let mut fresh_state = match InstanceState::new(self.pid, &next, state.context()) {
            Ok(fresh_state) => fresh_state,
            Err(why) => {
                self.set_state(LifecycleState::Active);
                return Err(UpgradeError::Instantiate(why));
            }
        };
        state.hand_over(&mut fresh_state);
        let rt = state.ctx.rt.clone();
        let outcome = match self.native_port(&rt, code, &next).await {
            Err(why) => Err(UpgradeError::Instantiate(why)),
            Ok(mut fresh) => {
                let loaded = self
                    .native_start(
                        &mut fresh_state,
                        &mut fresh,
                        &next,
                        Some((from_version, &snapshot)),
                    )
                    .await;
                let result = match loaded {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(refused)) => Err(UpgradeError::LoadFailed(refused)),
                    Err(reason) if self.kill.killed.load(Ordering::Acquire) => {
                        Err(UpgradeError::SaveFailed(reason))
                    }
                    Err(ExitReason::Crashed(why)) if why == "init_timeout" => {
                        Err(UpgradeError::LoadFailed("timed out".into()))
                    }
                    Err(reason) => Err(UpgradeError::LoadFailed(reason.to_string())),
                };
                match result {
                    Ok(()) => Ok(fresh),
                    Err(err) => {
                        fresh.kill();
                        fresh.finish().await;
                        Err(err)
                    }
                }
            }
        };
        match outcome {
            Ok(fresh) => {
                let mut old = std::mem::replace(port, fresh);
                self.me.set_native_memory(port.memory_source());
                old.kill();
                old.finish().await;
                *state = fresh_state;
                self.take_spec(&next);
                *spec = next;
                self.set_state(LifecycleState::Active);
                // The new process starts with fresh statics.
                self.native_announce(state);
                let pause = started.elapsed();
                tracing::info!(version = %spec.version, pause_us = pause.as_micros(), "upgraded");
                Ok(pause)
            }
            Err(err) => {
                // Give the instance's timers and waiting callers back.
                fresh_state.hand_over(state);
                if !matches!(err, UpgradeError::SaveFailed(_)) {
                    self.set_state(LifecycleState::Active);
                }
                tracing::warn!(error = %err, "upgrade reverted");
                Err(err)
            }
        }
    }

    /// Wait for the app's hello within the init timeout.
    async fn native_hello(&mut self, port: &mut Port, timeout: Duration) -> Result<(), ExitReason> {
        let hello = tokio::time::timeout(timeout, port.recv());
        let crashed = |why: String| Err(ExitReason::Crashed(why));
        match super::killable(&self.kill, &self.control, hello).await? {
            Err(_elapsed) => {
                port.kill();
                crashed("init_timeout".into())
            }
            Ok(Some(Incoming::Frame(FromApp::Hello { version }))) if p::accepts(version) => {
                port.set_version(version);
                Ok(())
            }
            Ok(Some(Incoming::Frame(FromApp::Hello { version }))) => {
                port.kill();
                crashed(format!(
                    "native protocol version {version}; this node accepts {} to {}",
                    p::MIN_VERSION,
                    p::VERSION
                ))
            }
            Ok(Some(Incoming::Frame(other))) => Err(broke(
                port,
                format!("expected hello, got {}", frame_name(&other)),
            )),
            Ok(Some(Incoming::Kv(_))) => Err(broke(port, "a request before hello".into())),
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
        // A timer left from an earlier callback is due no later than this
        // one's deadline, so it can only fire early: then the deadline is
        // checked and the timer moved, rather than armed for every frame.
        let timer = self
            .native_timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(start + budget)));
        if timer.deadline() > start + budget {
            timer.as_mut().reset(start + budget);
        }
        loop {
            let lease_end = lease_end(lease.as_ref());
            // A task this one woke since the last frame waits on this
            // worker, in its LIFO slot where nobody steals it: yield to it
            // rather than watch the rings (ROADMAP T9, B15).
            let watch = !self.me.take_woke();
            let step = super::killable(&self.kill, &self.control, async {
                tokio::select! {
                    biased;
                    () = timer.as_mut() => Step::Timer,
                    () = sleep_until(lease_end) => Step::LeaseEnded,
                    () = lease_changed(lease.as_mut()) => Step::LeaseChanged,
                    incoming = port.recv_in_callback(watch) => Step::Incoming(incoming),
                }
            })
            .await;
            let next = match step {
                Err(killed) => {
                    port.kill();
                    return Err(killed);
                }
                Ok(Step::Timer) => {
                    let deadline = start + budget + host;
                    if Instant::now() < deadline {
                        timer.as_mut().reset(deadline);
                        continue;
                    }
                    port.kill();
                    return Err(timeout.clone());
                }
                Ok(Step::LeaseEnded) => return Err(lease_ended(port)),
                Ok(Step::LeaseChanged) => continue,
                Ok(Step::Incoming(incoming)) => incoming,
            };
            match next {
                Some(Incoming::Frame(FromApp::Request {
                    id,
                    request: Request::OpenSender,
                })) => {
                    let answer = Answer::Sender(port.open_sender());
                    port.send(&ToApp::Answer { id, answer });
                }
                #[cfg(unix)]
                Some(Incoming::Frame(FromApp::Request {
                    id,
                    request: Request::Net(request),
                })) => {
                    let asked = Instant::now();
                    let asked_for = HostState::<Sockets>::service(state)
                        .state
                        .native(request, budget);
                    match super::killable(&self.kill, &self.control, asked_for).await {
                        Ok(reply) => {
                            let answer = Answer::Net(pass_socket(port, reply));
                            port.send(&ToApp::Answer { id, answer });
                        }
                        Err(killed) => {
                            port.kill();
                            return Err(killed);
                        }
                    }
                    host += asked.elapsed();
                }
                Some(Incoming::Frame(FromApp::Request { id, request })) => {
                    let asked = Instant::now();
                    match super::killable(&self.kill, &self.control, answer(state, request, budget))
                        .await
                    {
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
                Some(Incoming::Kv(call)) => {
                    let asked = Instant::now();
                    let id = call.id;
                    match super::killable(&self.kill, &self.control, kv_hot(state, call)).await {
                        Ok(Ok(outcome)) => send_kv(port, id, outcome),
                        Ok(Err(e)) => port.send(&ToApp::Answer {
                            id,
                            answer: Answer::Kv(Err(e)),
                        }),
                        Err(killed) => {
                            port.kill();
                            return Err(killed);
                        }
                    }
                    host += asked.elapsed();
                }
                Some(Incoming::Frame(FromApp::Log { level, message })) => {
                    log(state, level, &message);
                }
                Some(Incoming::Frame(FromApp::Pong { .. })) => {}
                Some(Incoming::Frame(FromApp::Metrics(updates))) => {
                    let refused = HostState::<MetricsService>::service(state)
                        .state
                        .native_updates(&updates);
                    if let Some(e) = refused {
                        log(
                            state,
                            p::Level::Warn,
                            &format!("metric updates refused: {e:?}"),
                        );
                    }
                }
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

    /// Handle messages until a stop is requested (`Ok`, with its reason)
    /// or the instance fails (`Err`).
    async fn native_serve(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &mut InstanceSpec,
        ping_interval: Duration,
        ping_timeout: Duration,
    ) -> Result<ExitReason, ExitReason> {
        let mut pings = Pings {
            in_flight: None,
            sent: 0,
            idle_since: Instant::now(),
            interval: ping_interval,
            timeout: ping_timeout,
        };
        // One timer, moved when it fires before a ping is due, rather than
        // armed for every message (Z1).
        let mut ping_timer =
            std::pin::pin!(tokio::time::sleep_until(pings.idle_since + ping_interval));
        // A singleton's lease. A native process can run threads between
        // callbacks, so a lease ending while it is idle kills it as one
        // ending in a callback does.
        let mut lease = state.lease.clone();
        let mut lease_timer = std::pin::pin!(tokio::time::sleep_until(
            lease_end(lease.as_ref()).unwrap_or_else(far_off)
        ));
        loop {
            match self.control.borrow_and_update().clone() {
                Control::Run => {}
                Control::Shutdown(reason) => return Ok(reason),
                Control::Kill(reason) => {
                    port.kill();
                    return Err(reason);
                }
            }
            let woke = {
                tokio::select! {
                    biased;
                    changed = self.control.changed() => {
                        if changed.is_err() {
                            port.kill();
                            return Err(ExitReason::Killed);
                        }
                        continue;
                    }
                    () = lease_timer.as_mut(), if lease.is_some() => {
                        lease_due(port, lease.as_ref(), lease_timer.as_mut())?;
                        continue;
                    }
                    () = lease_changed(lease.as_mut()) => {
                        lease_timer.as_mut().reset(lease_end(lease.as_ref()).unwrap_or_else(far_off));
                        continue;
                    }
                    () = published_changed(self.native_published.as_mut()) => {
                        self.native_publish(port, false);
                        continue;
                    }
                    Some(op) = self.ops.recv() => Woke::Op(op),
                    Some(info) = self.signals.recv() => match info.resolve() {
                        Some(info) => Woke::Next(Message::Info(info)),
                        None => continue,
                    },
                    message = self.mailbox.recv() => match message {
                        Some(message) => Woke::Next(message),
                        None => return Ok(ExitReason::Shutdown),
                    },
                    incoming = port.recv() => {
                        between_callbacks(state, port, incoming, &mut pings).await?;
                        continue;
                    }
                    () = ping_timer.as_mut() => {
                        pings.due(port, ping_timer.as_mut())?;
                        continue;
                    }
                }
            };
            let message = match woke {
                Woke::Next(message) => message,
                Woke::Op(op) => {
                    self.native_serve_op(state, port, spec, *op, &mut pings)
                        .await?;
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
            pings.answered();
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
        // Before the frame, so the app reads the hint after this callback.
        port.hint_idle(!self.has_work());
        // A task woken before this callback (the last caller, by its
        // reply) has run since: this callback's waits may watch the rings.
        // Kept, the flag made every callback of a call-reply pattern yield
        // instead, 2.4 to 2.6 µs to notice the app's reply (ROADMAP T12).
        let _ = self.me.take_woke();
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
                        if let Some(waiting) = state.take_reply(call_ref) {
                            let _ = waiting.send(convert::host_envelope(env));
                        }
                        Ok(())
                    }
                    Ok(FromApp::Reply(None)) => {
                        state.defer_reply();
                        state.check_pending().map_err(|e| {
                            port.kill();
                            ExitReason::Crashed(e.to_string())
                        })
                    }
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
                // A watch notice goes with the key's value (spike S31).
                let notice = match &info {
                    Info::HostEvent { name, msg } if name == WATCH_EVENT => {
                        HostState::<KeyValue>::service(state).native_notice(msg)
                    }
                    _ => None,
                };
                match notice.or_else(|| convert::info(info)) {
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

    /// An operation between callbacks. Returns the exit reason if the
    /// instance cannot continue. A rustbridge plugin takes no snapshot, so
    /// only announcements are done for one.
    async fn native_op(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &mut InstanceSpec,
        op: Op,
    ) -> Option<ExitReason> {
        let snapshots = self
            .app
            .as_ref()
            .map(LoadedApp::native)
            .is_some_and(|code| code.kind() == NativeProgram::Executable);
        match op {
            Op::Upgrade { reply, .. } if !snapshots => {
                let _ = reply.send(Err(UpgradeError::Invalid(
                    "a rustbridge plugin upgrades by restart or blue-green".into(),
                )));
                None
            }
            Op::Snapshot { reply } | Op::MoveOut { reply } if !snapshots => {
                let _ = reply.send(Err(SnapshotError::NotSupported));
                None
            }
            Op::Upgrade {
                app,
                spec: next,
                from_version,
                reply,
            } => {
                let result = self
                    .native_upgrade(state, port, spec, &app, *next, &from_version)
                    .await;
                let exit = match &result {
                    Err(UpgradeError::SaveFailed(reason)) => Some(reason.clone()),
                    _ => None,
                };
                if result.is_ok() {
                    self.app = Some(app);
                }
                let _ = reply.send(result);
                exit
            }
            Op::Snapshot { reply } => match self.native_save(state, port, spec).await {
                Ok(snapshot) => {
                    let _ = reply.send(Ok(snapshot));
                    None
                }
                Err(reason) => {
                    let _ = reply.send(Err(SnapshotError::Failed(reason.clone())));
                    Some(reason)
                }
            },
            // Moved out: the process handles nothing more, and ends with
            // the instance, without `terminate`.
            Op::MoveOut { reply } => match self.native_save(state, port, spec).await {
                Ok(snapshot) => {
                    let _ = reply.send(Ok(snapshot));
                    Some(self.frozen().await)
                }
                Err(reason) => {
                    let _ = reply.send(Err(SnapshotError::Failed(reason.clone())));
                    Some(reason)
                }
            },
            Op::Announce => {
                self.native_announce(state);
                None
            }
            Op::Connection(accepted) => self.native_connection(state, port, spec, accepted).await,
        }
    }

    /// Pass a connection the app's listener accepted and run its
    /// `handle_connection`. A stream that cannot be passed is closed; the
    /// instance carries on.
    #[cfg(unix)]
    async fn native_connection(
        &mut self,
        state: &mut InstanceState,
        port: &mut Port,
        spec: &InstanceSpec,
        accepted: Accepted,
    ) -> Option<ExitReason> {
        use std::os::fd::AsFd;
        let Accepted {
            listener,
            stream,
            tie,
        } = accepted;
        let sockets = &mut HostState::<Sockets>::service(state).state;
        let (mut stream, fd) = match sockets.connection(stream, tie) {
            Ok(kept) => kept,
            Err(e) => {
                tracing::warn!(%listener, error = ?e, "connection not passed");
                return None;
            }
        };
        match port.send_fd(fd.as_fd(), Passed::Stream) {
            Ok(passed) => stream.fd = passed,
            Err(why) => {
                tracing::warn!(%listener, error = %why, "connection not passed");
                let _ = sockets
                    .native(p::NetRequest::Close(stream.socket), Duration::ZERO)
                    .await;
                return None;
            }
        }
        drop(fd);
        probe::callback_start(self.pid, "connection");
        port.hint_idle(!self.has_work());
        let _ = self.me.take_woke();
        let started = Instant::now();
        port.send(&ToApp::Connection { listener, stream });
        let outcome = self.native_done(state, port, spec.callback_deadline).await;
        let took = started.elapsed();
        self.stats.callback("connection", took);
        if probe::callback_done_enabled() {
            let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
            probe::callback_done(self.pid, "connection", ns, outcome.is_ok());
        }
        outcome.err()
    }

    /// Native apps take connections on Unix only; elsewhere it closes.
    #[cfg(not(unix))]
    #[allow(clippy::unused_async)]
    async fn native_connection(
        &mut self,
        _state: &mut InstanceState,
        _port: &mut Port,
        _spec: &InstanceSpec,
        _accepted: Accepted,
    ) -> Option<ExitReason> {
        None
    }

    /// Queue the services' host events, ahead of mailbox messages.
    fn native_announce(&self, state: &InstanceState) {
        for (name, msg) in state.announcements() {
            self.me.signal(Info::HostEvent { name, msg });
        }
    }
}
