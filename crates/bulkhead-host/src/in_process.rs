//! In-process test apps (cargo feature `test-apps`, ROADMAP Z9c): an app
//! written in Rust that runs inside its instance's own task, for tests that
//! need many instances, quick starts or tokio's paused clock (the
//! simulation), which a process cannot follow.
//!
//! The app speaks the native protocol's frames, so the instance runs the
//! native kind's lifecycle (hello, init, callbacks, snapshots, pings,
//! deadlines, the node's request dispatch) unchanged; only the transport
//! differs. Its code is an async Rust value the instance polls while it
//! waits for a frame, and its requests to the node ([`TestCtx`]) are
//! frames whose answers it awaits. Nothing runs on another thread, so a
//! paused clock moves only when the instance's task is idle.
//!
//! An in-process app cannot be stopped while its code runs without
//! awaiting: a spin holds the worker. Tests of deadlines, kills of runaway
//! code, memory limits and the sandbox use native processes. A panic in its
//! code is not caught; an app ends a callback with an error instead
//! ([`TestApp::handle_call`] returning `Err`), which the instance sees as a
//! native app's panic (`crashed: panic: ...`).
//!
//! ```ignore
//! struct Echo;
//!
//! impl TestApp for Echo {
//!     async fn init(_cx: &mut TestCtx, _args: Vec<u8>) -> Result<Self, String> {
//!         Ok(Echo)
//!     }
//!     async fn handle_call(&mut self, _cx: &mut TestCtx, _call: u64, msg: Envelope)
//!         -> Result<Option<Envelope>, String> {
//!         Ok(Some(msg))
//!     }
//! }
//!
//! let app = runtime.load_in_process::<Echo>();
//! ```

pub mod chaos;
pub mod conformance;
pub mod counter;
pub mod services;

use std::collections::VecDeque;
use std::fmt;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use bulkhead_native::protocol::{self as p, FromApp, ToApp};
pub use bulkhead_native::protocol::{
    Answer, CallError, CastError, Envelope, ExitReason, Info, KvAnswer, KvError, KvRequest, Pid,
    RegisterError, Request,
};

use crate::message::ExitReason as HostExitReason;
use crate::native::port::Incoming;

/// An app run in its instance's task. Its methods are its callbacks, as
/// `bulkhead_native::app::App`'s are, but async: requests to the node
/// ([`TestCtx`]) are awaited. A method's `Err` ends the app as a native
/// app's panic does.
pub trait TestApp: Sized + Send + 'static {
    /// Start, from the manifest's `start_args` (or the spec's).
    fn init(cx: &mut TestCtx, args: Vec<u8>) -> impl Future<Output = Result<Self, String>> + Send;

    /// Start from a snapshot `save_snapshot` wrote, from `from_version`.
    /// Refused by default.
    fn load_snapshot(
        cx: &mut TestCtx,
        from_version: String,
        snapshot: Vec<u8>,
    ) -> impl Future<Output = Result<Self, String>> + Send {
        let _ = (cx, from_version, snapshot);
        async { Err("this app takes no snapshot".to_string()) }
    }

    /// Answer a call: `Some` now, `None` for a reply later
    /// ([`TestCtx::reply`] with `call_ref`).
    fn handle_call(
        &mut self,
        cx: &mut TestCtx,
        call_ref: u64,
        msg: Envelope,
    ) -> impl Future<Output = Result<Option<Envelope>, String>> + Send;

    /// Handle a cast. Nothing by default.
    fn handle_cast(
        &mut self,
        cx: &mut TestCtx,
        msg: Envelope,
    ) -> impl Future<Output = Result<(), String>> + Send {
        let _ = (cx, msg);
        async { Ok(()) }
    }

    /// Handle an exit signal (when trapping), a monitor's down notice, a
    /// timer's message or a host event. Nothing by default.
    fn handle_info(
        &mut self,
        cx: &mut TestCtx,
        info: Info,
    ) -> impl Future<Output = Result<(), String>> + Send {
        let _ = (cx, info);
        async { Ok(()) }
    }

    /// The instance is stopping. Nothing by default.
    fn terminate(
        &mut self,
        cx: &mut TestCtx,
        reason: ExitReason,
    ) -> impl Future<Output = ()> + Send {
        let _ = (cx, reason);
        async {}
    }

    /// The state to carry through a snapshot upgrade, passivation or a
    /// move. Refused by default.
    ///
    /// # Errors
    ///
    /// Why it cannot be saved.
    fn save_snapshot(&mut self) -> Result<Vec<u8>, String> {
        Err("this app takes no snapshot".into())
    }
}

/// The frames between an in-process app and its port.
#[derive(Default)]
struct Queues {
    /// From the node, not yet taken by the app.
    inbox: VecDeque<ToApp>,
    /// From the app, not yet taken by the node.
    outbox: VecDeque<FromApp>,
    /// The node will send no more.
    closed: bool,
    /// The task to wake when a frame for the app arrives.
    waker: Option<Waker>,
}

type Pipe = Arc<Mutex<Queues>>;

fn lock(pipe: &Pipe) -> std::sync::MutexGuard<'_, Queues> {
    pipe.lock().unwrap_or_else(PoisonError::into_inner)
}

fn emit(pipe: &Pipe, frame: FromApp) {
    lock(pipe).outbox.push_back(frame);
}

/// The next frame from the node; `None` once it has closed the app's end.
async fn next(pipe: &Pipe) -> Option<ToApp> {
    poll_fn(|cx| {
        let mut q = lock(pipe);
        if let Some(frame) = q.inbox.pop_front() {
            return Poll::Ready(Some(frame));
        }
        if q.closed {
            return Poll::Ready(None);
        }
        q.waker = Some(cx.waker().clone());
        Poll::Pending
    })
    .await
}

/// What an in-process app reaches during a callback: requests to the node,
/// each a frame whose answer it awaits.
pub struct TestCtx {
    pipe: Pipe,
    next_id: u64,
}

impl fmt::Debug for TestCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestCtx")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl TestCtx {
    /// Ask the node: the answer, or [`Answer::Unit`] if the node closed the
    /// app's end first.
    pub async fn request(&mut self, request: Request) -> Answer {
        self.next_id += 1;
        let id = self.next_id;
        emit(&self.pipe, FromApp::Request { id, request });
        while let Some(frame) = next(&self.pipe).await {
            match frame {
                ToApp::Answer { id: got, answer } if got == id => return answer,
                ToApp::Ping { id } => emit(&self.pipe, FromApp::Pong { id }),
                // The node sends nothing else during a callback.
                _ => {}
            }
        }
        Answer::Unit
    }

    /// This instance's pid.
    pub async fn self_pid(&mut self) -> Pid {
        match self.request(Request::SelfPid).await {
            Answer::Pid(pid) => pid,
            _ => Pid {
                node: 0,
                id: 0,
                incarnation: 0,
            },
        }
    }

    /// Call `to` and wait up to `timeout_ms` for its reply.
    ///
    /// # Errors
    ///
    /// Why no reply came.
    pub async fn call(
        &mut self,
        to: Pid,
        msg: Envelope,
        timeout_ms: u32,
    ) -> Result<Envelope, CallError> {
        match self
            .request(Request::Call {
                to,
                msg,
                timeout_ms,
            })
            .await
        {
            Answer::Called(result) => result,
            _ => Err(CallError::Noproc),
        }
    }

    /// Cast to `to`.
    ///
    /// # Errors
    ///
    /// Why it was not queued.
    pub async fn cast(&mut self, to: Pid, msg: Envelope) -> Result<(), CastError> {
        match self.request(Request::Cast { to, msg }).await {
            Answer::CastFailed(e) => Err(e),
            _ => Ok(()),
        }
    }

    /// Reply to the call `call_ref` later.
    pub async fn reply(&mut self, call_ref: u64, msg: Envelope) {
        self.request(Request::Reply { call_ref, msg }).await;
    }

    /// Link to `other`.
    pub async fn link(&mut self, other: Pid) {
        self.request(Request::Link(other)).await;
    }

    /// Remove a link to `other`.
    pub async fn unlink(&mut self, other: Pid) {
        self.request(Request::Unlink(other)).await;
    }

    /// Trap exit signals as messages, or not.
    pub async fn trap_exits(&mut self, on: bool) {
        self.request(Request::TrapExits(on)).await;
    }

    /// Monitor `other`: its reference.
    pub async fn monitor(&mut self, other: Pid) -> u64 {
        match self.request(Request::Monitor(other)).await {
            Answer::Ref(r) => r,
            _ => 0,
        }
    }

    /// Remove the monitor `reference`.
    pub async fn demonitor(&mut self, reference: u64) {
        self.request(Request::Demonitor(reference)).await;
    }

    /// Register this instance as `name`.
    ///
    /// # Errors
    ///
    /// Why it was refused.
    pub async fn register(&mut self, name: &str) -> Result<(), RegisterError> {
        match self.request(Request::Register(name.to_string())).await {
            Answer::Registered(r) => r,
            _ => Err(RegisterError::AccessDenied),
        }
    }

    /// The pid registered as `name`.
    pub async fn whereis(&mut self, name: &str) -> Option<Pid> {
        match self.request(Request::Whereis(name.to_string())).await {
            Answer::MaybePid(pid) => pid,
            _ => None,
        }
    }

    /// Send `msg` to this instance after `ms`: the timer's reference.
    pub async fn send_after(&mut self, ms: u32, msg: Envelope) -> u64 {
        match self.request(Request::SendAfter { ms, msg }).await {
            Answer::Ref(r) => r,
            _ => 0,
        }
    }

    /// Cancel the timer `reference`.
    pub async fn cancel_timer(&mut self, reference: u64) {
        self.request(Request::CancelTimer(reference)).await;
    }

    /// Send an exit signal to `to`.
    pub async fn exit(&mut self, to: Pid, reason: ExitReason) {
        self.request(Request::Exit { to, reason }).await;
    }

    /// A process groups operation.
    ///
    /// # Errors
    ///
    /// As the operation failed.
    pub async fn groups(
        &mut self,
        request: p::GroupsRequest,
    ) -> Result<p::GroupsAnswer, p::GroupsError> {
        match self.request(Request::Groups(request)).await {
            Answer::Groups(r) => r,
            _ => Err(p::GroupsError::AccessDenied),
        }
    }

    /// A keyvalue operation.
    ///
    /// # Errors
    ///
    /// As the operation failed.
    pub async fn kv(&mut self, request: KvRequest) -> Result<KvAnswer, KvError> {
        match self.request(Request::Kv(request)).await {
            Answer::Kv(r) => r,
            _ => Err(KvError::Other("no answer".into())),
        }
    }
}

/// How an in-process app ended: as a process's exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// Its loop returned, with this status.
    Exited(i32),
    /// The node dropped it.
    Killed,
}

type AppFuture = Pin<Box<dyn Future<Output = Ended> + Send>>;

/// A loaded in-process app: starts its loop for an instance.
#[derive(Clone)]
pub(crate) struct InProcessApp {
    name: &'static str,
    start: Arc<dyn Fn(Pipe) -> AppFuture + Send + Sync>,
}

impl fmt::Debug for InProcessApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InProcessApp")
            .field("app", &self.name)
            .finish_non_exhaustive()
    }
}

impl InProcessApp {
    /// The app `A`.
    pub(crate) fn of<A: TestApp>() -> Self {
        Self {
            name: std::any::type_name::<A>(),
            start: Arc::new(|pipe| Box::pin(serve::<A>(pipe))),
        }
    }

    /// A new instance of the app, with its port.
    pub(crate) fn start(&self) -> InProcessPort {
        let pipe = Pipe::default();
        InProcessPort {
            app: Some((self.start)(pipe.clone())),
            pipe,
            ended: None,
        }
    }
}

/// The app's loop, as `bulkhead_native::app::serve` runs a process's.
async fn serve<A: TestApp>(pipe: Pipe) -> Ended {
    emit(
        &pipe,
        FromApp::Hello {
            version: p::VERSION,
        },
    );
    let mut cx = TestCtx {
        pipe: pipe.clone(),
        next_id: 0,
    };
    let first = loop {
        match next(&pipe).await {
            Some(ToApp::Ping { id }) => emit(&pipe, FromApp::Pong { id }),
            Some(frame) => break frame,
            None => return Ended::Exited(0),
        }
    };
    let started = match first {
        ToApp::Init { args } => A::init(&mut cx, args).await,
        ToApp::LoadSnapshot {
            from_version,
            snapshot,
        } => A::load_snapshot(&mut cx, from_version, snapshot).await,
        other => {
            emit(
                &pipe,
                FromApp::Failed(format!("expected init, got {other:?}")),
            );
            return Ended::Exited(2);
        }
    };
    let mut app = match started {
        Ok(app) => {
            emit(&pipe, FromApp::Init(Ok(())));
            app
        }
        Err(why) => {
            emit(&pipe, FromApp::Init(Err(why)));
            return Ended::Exited(1);
        }
    };
    let failed = |pipe: &Pipe, why: String| {
        emit(pipe, FromApp::Failed(format!("panic: {why}")));
        Ended::Exited(101)
    };
    while let Some(frame) = next(&pipe).await {
        match frame {
            ToApp::Call { call_ref, msg } => match app.handle_call(&mut cx, call_ref, msg).await {
                Ok(reply) => emit(&pipe, FromApp::Reply(reply)),
                Err(why) => return failed(&pipe, why),
            },
            ToApp::Cast { msg } => match app.handle_cast(&mut cx, msg).await {
                Ok(()) => emit(&pipe, FromApp::Done),
                Err(why) => return failed(&pipe, why),
            },
            ToApp::Info { info } => match app.handle_info(&mut cx, info).await {
                Ok(()) => emit(&pipe, FromApp::Done),
                Err(why) => return failed(&pipe, why),
            },
            // No descriptors pass to an in-process app: it has no stream.
            ToApp::Connection { .. } => emit(&pipe, FromApp::Done),
            ToApp::SaveSnapshot => match app.save_snapshot() {
                Ok(bytes) => emit(&pipe, FromApp::Snapshot(p::Bytes(bytes))),
                Err(why) => {
                    emit(
                        &pipe,
                        FromApp::Failed(format!("save_snapshot failed: {why}")),
                    );
                    return Ended::Exited(101);
                }
            },
            ToApp::Terminate { reason } => {
                app.terminate(&mut cx, reason).await;
                emit(&pipe, FromApp::Done);
                return Ended::Exited(0);
            }
            ToApp::Ping { id } => emit(&pipe, FromApp::Pong { id }),
            ToApp::Published { .. } => {}
            other => {
                emit(
                    &pipe,
                    FromApp::Failed(format!("unexpected {other:?} between callbacks")),
                );
                return Ended::Exited(2);
            }
        }
    }
    Ended::Exited(0)
}

/// An in-process app's end of the instance: the port's interface over the
/// app's frames, the app polled whenever the instance waits for one.
pub(crate) struct InProcessPort {
    pipe: Pipe,
    app: Option<AppFuture>,
    ended: Option<Ended>,
}

impl fmt::Debug for InProcessPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InProcessPort")
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl InProcessPort {
    pub(crate) fn set_version(&mut self, _version: u16) {}

    pub(crate) fn send(&mut self, frame: &ToApp) {
        let mut q = lock(&self.pipe);
        if q.closed {
            return;
        }
        q.inbox.push_back(frame.clone());
        if let Some(waker) = q.waker.take() {
            waker.wake();
        }
    }

    /// An in-process app sends no hot frames, so is never answered with
    /// one.
    pub(crate) fn send_kv(&mut self, _id: u64, _reply: &bulkhead_native::hot::KvReply<'_>) {}

    pub(crate) fn send_published(
        &mut self,
        _version: &crate::services::keyvalue::publish::Version,
    ) -> Result<p::FdRef, String> {
        Err("an in-process app takes no descriptors".into())
    }

    #[cfg(unix)]
    pub(crate) fn send_fd(&mut self, _fd: std::os::fd::BorrowedFd<'_>) -> Result<p::FdRef, String> {
        Err("an in-process app takes no descriptors".into())
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Incoming>> {
        loop {
            if let Some(frame) = lock(&self.pipe).outbox.pop_front() {
                return Poll::Ready(Some(Incoming::Frame(frame)));
            }
            let Some(app) = self.app.as_mut() else {
                return Poll::Ready(None);
            };
            match app.as_mut().poll(cx) {
                Poll::Ready(ended) => {
                    self.ended = Some(ended);
                    self.app = None;
                }
                Poll::Pending => {
                    if lock(&self.pipe).outbox.is_empty() {
                        return Poll::Pending;
                    }
                }
            }
        }
    }

    pub(crate) async fn recv(&mut self) -> Option<Incoming> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub(crate) fn hint_idle(&mut self, _nothing_queued: bool) {}

    pub(crate) async fn recv_in_callback(&mut self, _watch: bool) -> Option<Incoming> {
        self.recv().await
    }

    fn close(&mut self) {
        let mut q = lock(&self.pipe);
        q.closed = true;
        if let Some(waker) = q.waker.take() {
            waker.wake();
        }
    }

    pub(crate) fn kill(&mut self) {
        if self.app.take().is_some() {
            self.ended = Some(Ended::Killed);
        }
    }

    pub(crate) async fn exit_reason(&mut self) -> HostExitReason {
        // A closed app's loop ends at its next frame; one still running is
        // dropped, as a process would be killed after its grace period.
        if self.app.is_some() {
            self.close();
            while self.recv().await.is_some() {}
        }
        self.reason()
    }

    pub(crate) fn exited_reason(&mut self) -> Option<HostExitReason> {
        self.ended.map(|_| self.reason())
    }

    fn reason(&self) -> HostExitReason {
        HostExitReason::Crashed(match self.ended {
            Some(Ended::Exited(code)) => format!("exited with status {code}"),
            Some(Ended::Killed) => "killed by signal 9".into(),
            None => "exited".into(),
        })
    }

    pub(crate) async fn finish(&mut self) {
        self.close();
        self.app = None;
    }
}
