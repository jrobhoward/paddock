//! Sender channels (ROADMAP AP1, spike S39): a channel of its own for each
//! of a native app's threads that asks the node outside the callbacks.
//!
//! The app asks for one with `Request::OpenSender`, from a callback or
//! from a sender it holds. The node makes a channel as it makes the
//! instance's own (two rings in a memfd, a door, a bell), passes the app's
//! ends on the instance's descriptor socket and answers with their ids.
//! A task of its own then serves the channel, not the instance's task: a
//! sender's request is answered at any time, never behind the running
//! callback or a slow request that callback made. S39 measured a channel
//! per thread costing what a callback's request costs, where a ring shared
//! by threads paid a hand-off for each answer and a lock among senders.
//!
//! The task holds no `InstanceState`: casts and calls go through
//! [`delivery`] from the instance (its pid, its messaging ACLs), a call's
//! reply coming back on the call's own channel rather than the instance's
//! mailbox; keyvalue and metrics run on state of the channel's own, made
//! from the instance's spec and grants ([`SenderStates`]). Calls and casts
//! that wait run concurrently, up to [`IN_FLIGHT_MAX`] on one channel,
//! each answered by its id as it completes; the rest are answered in
//! order. A request a sender may not make ends the instance, as a request
//! between callbacks does.
//!
//! A process's senders end with it ([`Senders`] is held by its port), and
//! one ends when the app closes its ring or its end of the bell, which
//! frees its place under `senders_max`.

use std::future::Future;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bulkhead_native::channel::{self, Channel};
use bulkhead_native::hot::KvReply;
use bulkhead_native::protocol::{
    self as p, Answer, FdRef, FromApp, KvRequest, Request, SenderChannel, SenderError, ToApp,
};
use tokio::task::AbortHandle;

use super::convert;
use super::link::Link;
use super::port::{Incoming, KvCall};
use crate::config::HostObjects;
use crate::delivery::{self, Origin};
use crate::instance::InstanceHandle;
use crate::message::{CallError, ExitReason};
use crate::metrics::AppStats;
use crate::native::{Passed, os};
use crate::output::Tags;
use crate::probe;
use crate::runtime::RuntimeInner;
use crate::services::keyvalue::storage::Outcome;
use crate::services::{SenderStates, ServiceCtx};
use crate::spec::InstanceSpec;

/// Most requests one sender channel has in flight; past it the channel's
/// task reads no more frames until one completes, so the app's ring fills
/// and its thread waits.
pub(crate) const IN_FLIGHT_MAX: usize = 64;

/// The node's end of an instance's descriptor socket, shared by the
/// instance's task and its senders' tasks. Each descriptor is one
/// `SOCK_SEQPACKET` message with its id, so two tasks passing at once
/// cannot mix theirs.
#[derive(Debug)]
pub(crate) struct FdPass {
    fd: OwnedFd,
    next: AtomicU64,
}

impl FdPass {
    pub(crate) fn new(fd: OwnedFd) -> Self {
        Self {
            fd,
            next: AtomicU64::new(1),
        }
    }

    /// Pass `fd` to the app as `what`, for a frame sent after this to
    /// name.
    ///
    /// # Errors
    ///
    /// The socket refused it: full (an app that takes no descriptors), or
    /// the app gone.
    pub(crate) fn send(&self, fd: BorrowedFd<'_>, what: Passed) -> Result<FdRef, String> {
        let copy = os::for_app(fd, what).map_err(|e| format!("limit a descriptor: {e}"))?;
        let fd = copy.as_ref().map_or(fd, AsFd::as_fd);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        channel::send_fd(self.fd.as_fd(), id, fd).map_err(|e| format!("pass a descriptor: {e}"))?;
        Ok(FdRef(id))
    }
}

/// What a sender's task acts as: the instance, its runtime and its spec.
pub(crate) struct SenderContext {
    pub(crate) rt: Arc<RuntimeInner>,
    pub(crate) me: InstanceHandle,
    pub(crate) tags: Arc<Tags>,
    pub(crate) stats: Arc<AppStats>,
    pub(crate) spec: InstanceSpec,
    pub(crate) host_objects: HostObjects,
    pub(crate) spin: Duration,
}

/// A process's sender channels: held by its port, and every sender's task
/// ended when it drops.
#[derive(Debug)]
pub(crate) struct Senders(Arc<Hub>);

impl Senders {
    pub(crate) fn new(ctx: SenderContext, fds: Arc<FdPass>) -> Self {
        Self(Arc::new(Hub {
            max: ctx.host_objects.senders,
            ctx,
            fds,
            open: AtomicU32::new(0),
            tasks: Mutex::new(Vec::new()),
        }))
    }

    /// Make a sender channel and start its task.
    pub(crate) fn open(&self) -> Result<SenderChannel, SenderError> {
        Hub::open(&self.0)
    }
}

impl Drop for Senders {
    fn drop(&mut self) {
        let tasks =
            std::mem::take(&mut *self.0.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        for task in tasks {
            task.abort();
        }
    }
}

struct Hub {
    ctx: SenderContext,
    fds: Arc<FdPass>,
    max: u32,
    /// Channels open, counted against `max`.
    open: AtomicU32,
    /// The tasks of the channels opened, finished ones included until
    /// the next open prunes them.
    tasks: Mutex<Vec<AbortHandle>>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("pid", &self.ctx.tags.pid)
            .field("open", &self.open)
            .field("max", &self.max)
            .finish_non_exhaustive()
    }
}

/// A place under `senders_max`, given back when dropped: when the
/// channel's task ends, or when the channel could not be made.
struct Place(Arc<Hub>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Hub {
    fn open(hub: &Arc<Self>) -> Result<SenderChannel, SenderError> {
        let mut open = hub.open.load(Ordering::SeqCst);
        loop {
            if open >= hub.max {
                return Err(SenderError::LimitReached);
            }
            match hub
                .open
                .compare_exchange_weak(open, open + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(now) => open = now,
            }
        }
        let place = Place(hub.clone());
        let other = |e: std::io::Error| SenderError::Other(format!("sender channel: {e}"));
        let (channel, memfd) = Channel::create().map_err(other)?;
        let (bell, bell_app) = channel::bell().map_err(other)?;
        let (door_app, door) = channel::door().map_err(other)?;
        let link = Link::new(channel, bell, door, hub.ctx.spin).map_err(SenderError::Other)?;
        // The app's ends go now and the node keeps none, so the bell's end
        // of file means the app closed its sender.
        let passed = SenderChannel {
            memfd: hub
                .fds
                .send(memfd.as_fd(), Passed::Memfd)
                .map_err(SenderError::Other)?,
            door: hub
                .fds
                .send(door_app.as_fd(), Passed::Door)
                .map_err(SenderError::Other)?,
            bell: hub
                .fds
                .send(bell_app.as_fd(), Passed::Bell)
                .map_err(SenderError::Other)?,
        };
        drop((memfd, door_app, bell_app));
        let states = SenderStates::new(
            &hub.ctx.rt.services,
            &hub.ctx.spec,
            &hub.ctx.me,
            hub.ctx.host_objects,
            &hub.ctx.rt.metrics,
        );
        let task = tokio::spawn(serve(place, link, states));
        let mut tasks = hub.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.retain(|t| !t.is_finished());
        tasks.push(task.abort_handle());
        Ok(passed)
    }
}

/// A request answered later: its id and its answer.
type InFlight = Pin<Box<dyn Future<Output = (u64, Answer)> + Send>>;

/// Serve one sender channel until the app closes it, it breaks the
/// protocol, or the process's port drops.
async fn serve(place: Place, mut link: Link, mut states: SenderStates) {
    let hub = place.0.clone();
    let mut in_flight: Vec<InFlight> = Vec::new();
    loop {
        let next = tokio::select! {
            biased;
            (id, answer) = next_done(&mut in_flight), if !in_flight.is_empty() => {
                link.send(&ToApp::Answer { id, answer });
                continue;
            }
            // As inside a callback: the thread waits for its answer, so
            // the wait watches the rings within the poll while the worker
            // has nothing else to do (T9), rather than yield and wake a
            // parked worker for nothing.
            incoming = link.recv_in_callback(true), if in_flight.len() < IN_FLIGHT_MAX => incoming,
        };
        match next {
            Some(Incoming::Frame(FromApp::Request { id, request })) => {
                match handle(&hub, &mut states, request) {
                    Handled::Now(answer) => link.send(&ToApp::Answer { id, answer }),
                    Handled::Later(fut) => {
                        let fut: InFlight = Box::pin(async move { (id, fut.await) });
                        in_flight.push(fut);
                    }
                    Handled::Kv(request) => {
                        let tags = hub.ctx.tags.clone();
                        let answer = ServiceCtx::new(&mut states.keyvalue, &tags)
                            .native(request)
                            .await;
                        link.send(&ToApp::Answer {
                            id,
                            answer: Answer::Kv(answer),
                        });
                    }
                    Handled::Refused(what) => {
                        refuse(&hub, &what);
                        return;
                    }
                }
            }
            Some(Incoming::Kv(call)) => kv_hot(&hub, &mut link, &mut states, call).await,
            Some(Incoming::Frame(FromApp::Metrics(updates))) => {
                if let Some(e) = states.metrics.native_updates(&updates) {
                    tracing::warn!(
                        app = %hub.ctx.tags.app,
                        pid = %hub.ctx.tags.pid,
                        "metric updates from a sender refused: {e:?}"
                    );
                }
            }
            Some(Incoming::Frame(other)) => {
                refuse(&hub, &format!("{other:?}"));
                return;
            }
            Some(Incoming::Broken(why)) => {
                hub.ctx
                    .me
                    .kill_with(ExitReason::Crashed(format!("sender: {why}")));
                return;
            }
            // The app closed the sender, or exited.
            None => return,
        }
    }
}

/// The first request in flight to complete, removed from the list.
async fn next_done(in_flight: &mut Vec<InFlight>) -> (u64, Answer) {
    std::future::poll_fn(|cx| {
        for i in 0..in_flight.len() {
            if let std::task::Poll::Ready(done) = in_flight[i].as_mut().poll(cx) {
                drop(in_flight.swap_remove(i));
                return std::task::Poll::Ready(done);
            }
        }
        std::task::Poll::Pending
    })
    .await
}

/// A request a sender may not make, or a frame it may not send: the
/// instance ends.
fn refuse(hub: &Hub, what: &str) {
    hub.ctx.me.kill_with(ExitReason::Crashed(format!(
        "sender: not allowed on a sender channel: {what}"
    )));
}

/// How a sender's request is answered.
enum Handled {
    Now(Answer),
    Later(Pin<Box<dyn Future<Output = Answer> + Send>>),
    /// A keyvalue request, answered in order on the channel's own state.
    Kv(KvRequest),
    Refused(String),
}

fn handle(hub: &Arc<Hub>, states: &mut SenderStates, request: Request) -> Handled {
    let ctx = &hub.ctx;
    match request {
        Request::SelfPid => Handled::Now(Answer::Pid(convert::pid(ctx.tags.pid))),
        Request::Cast { to, msg } => {
            let sent = delivery::cast(
                &ctx.rt,
                Origin::Local(&ctx.me),
                convert::host_pid(to),
                convert::host_envelope(msg),
            );
            Handled::Now(cast_answer(sent))
        }
        Request::CastWait {
            to,
            msg,
            timeout_ms,
        } => {
            let hub = hub.clone();
            Handled::Later(Box::pin(async move {
                let ctx = &hub.ctx;
                let to = convert::host_pid(to);
                let started = Instant::now();
                let sent = delivery::cast_wait(
                    &ctx.rt,
                    Origin::Local(&ctx.me),
                    to,
                    convert::host_envelope(msg),
                    Duration::from_millis(timeout_ms.into()),
                )
                .await;
                let waited = started.elapsed();
                ctx.stats.cast_waited(waited);
                probe::cast_waited(ctx.tags.pid, to, waited, &sent);
                cast_answer(sent)
            }))
        }
        Request::Call {
            to,
            msg,
            timeout_ms,
        } => {
            let hub = hub.clone();
            Handled::Later(Box::pin(async move {
                let ctx = &hub.ctx;
                let result = delivery::call(
                    &ctx.rt,
                    Origin::Local(&ctx.me),
                    convert::host_pid(to),
                    convert::host_envelope(msg),
                    Duration::from_millis(timeout_ms.into()),
                )
                .await;
                Answer::Called(
                    result
                        .map(convert::envelope)
                        .map_err(|e| convert::call_error(&e)),
                )
            }))
        }
        Request::CallApp {
            app,
            msg,
            timeout_ms,
        } => {
            let hub = hub.clone();
            Handled::Later(Box::pin(async move {
                let ctx = &hub.ctx;
                let msg = convert::host_envelope(msg);
                let result = match ctx.rt.route_app(&app, &msg) {
                    Some(target) => {
                        delivery::call(
                            &ctx.rt,
                            Origin::Local(&ctx.me),
                            target.pid(),
                            msg,
                            Duration::from_millis(timeout_ms.into()),
                        )
                        .await
                    }
                    None => Err(CallError::Noproc),
                };
                Answer::Called(
                    result
                        .map(convert::envelope)
                        .map_err(|e| convert::call_error(&e)),
                )
            }))
        }
        // A watch's notices go to the instance's mailbox and count against
        // its `watches_max`: watches are the callback thread's.
        Request::Kv(KvRequest::Watch { .. } | KvRequest::Unwatch(_)) => {
            Handled::Now(Answer::Kv(Err(p::KvError::AccessDenied)))
        }
        Request::Kv(request) => Handled::Kv(request),
        Request::KvOpen(bucket) => Handled::Now(Answer::KvOpened(
            ServiceCtx::new(&mut states.keyvalue, &ctx.tags).native_open(&bucket),
        )),
        Request::MetricRegister {
            kind,
            name,
            help,
            labels,
            buckets,
        } => Handled::Now(Answer::Metric(
            states
                .metrics
                .native_register(kind, &name, &help, &labels, &buckets),
        )),
        Request::OpenSender => Handled::Now(Answer::Sender(Hub::open(hub))),
        other => Handled::Refused(format!("{other:?}")),
    }
}

/// A hot frame's keyvalue request, answered in a hot frame.
async fn kv_hot(hub: &Hub, link: &mut Link, states: &mut SenderStates, call: KvCall) {
    let outcome = ServiceCtx::new(&mut states.keyvalue, &hub.ctx.tags)
        .native_hot(call.bucket, call.op, call.key, call.value)
        .await;
    match outcome {
        Ok(outcome) => match &outcome {
            Outcome::Value(v) => link.send_kv(call.id, &KvReply::Value(v.as_deref())),
            Outcome::Exists(e) => link.send_kv(call.id, &KvReply::Exists(*e)),
            Outcome::Done => link.send_kv(call.id, &KvReply::Unit),
            other => link.send(&ToApp::Answer {
                id: call.id,
                answer: Answer::Kv(Err(p::KvError::Other(format!(
                    "unexpected outcome {other:?}"
                )))),
            }),
        },
        Err(e) => link.send(&ToApp::Answer {
            id: call.id,
            answer: Answer::Kv(Err(e)),
        }),
    }
}

/// A cast's answer: `Unit` once queued, or why it was not.
fn cast_answer(sent: Result<(), CallError>) -> Answer {
    match sent {
        Ok(()) => Answer::Unit,
        Err(err) => Answer::CastFailed(convert::cast_error(&err)),
    }
}
