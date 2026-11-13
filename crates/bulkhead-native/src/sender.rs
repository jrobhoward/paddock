//! A channel of its own for one of the app's threads (ROADMAP AP1, spike
//! S39): a [`Sender`] asks the node from any thread, at any time, not only
//! from the callback thread during a callback.
//!
//! The callback thread opens one with [`Ctx::sender`](crate::app::Ctx::sender),
//! and a thread holding one opens more with [`Sender::try_clone`]. Each is
//! its own pair of rings with its own door and bell, used by one thread at
//! a time (`Send`, not `Sync`), so a request costs what a callback's does
//! and no thread waits on another's. The node serves each sender with a
//! task of its own, as the instance (its pid, its messaging ACLs, its
//! grants), never behind the callback that is running.
//!
//! A sender asks for messaging (casts and calls), keyvalue (not watches)
//! and metrics. Calls may be in flight together: [`Sender::start_call`]
//! sends one and [`Sender::next_answer`] takes the answers as they come,
//! up to 64 on one sender before the node reads no more of its requests.
//! Metric updates go at once, one frame each, with no answer. Dropping a
//! sender closes its end of the bell, which ends the node's task for it
//! and gives its place under the app's `senders_max` back.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::marker::PhantomData;

use crate::app::{Ctx, take_fd};
use crate::channel::{AppEnd, FromNode, Spin, ToNode};
use crate::hot::{KvFrame, KvOp, KvReply};
use crate::protocol::{
    self, Answer, Bytes, CallError, CastError, Envelope, FromApp, KvAnswer, KvError, KvRequest,
    MetricError, MetricKind, MetricUpdate, Pid, Request, SenderChannel, SenderError, ToApp,
};

/// One thread's channel to the node.
#[derive(Debug)]
pub struct Sender {
    end: AppEnd,
    next_id: u64,
    /// Bucket handles the node gave this sender, by bucket name.
    handles: HashMap<String, u32>,
    /// Calls started and not yet taken by [`Sender::next_answer`], and the
    /// answers that came for them while a request waited.
    started: HashMap<u64, Option<Result<Envelope, CallError>>>,
    out: Vec<u8>,
    inbuf: Vec<u8>,
    /// Not `Sync`: one thread at a time.
    _one_thread: PhantomData<std::cell::Cell<()>>,
}

/// What came from the node.
enum Got {
    Frame(ToApp),
    /// A hot keyvalue answer, in `inbuf`.
    Hot,
}

impl Sender {
    /// The sender the node made, from the answer naming its descriptors.
    pub(crate) fn from_channel(passed: SenderChannel) -> Result<Self, SenderError> {
        let other = |e: io::Error| SenderError::Other(format!("sender channel: {e}"));
        let memfd = take_fd(passed.memfd).map_err(other)?;
        let door = take_fd(passed.door).map_err(other)?;
        let bell = take_fd(passed.bell).map_err(other)?;
        let end = AppEnd::sender(memfd, bell, door, Spin::sender_from_env()).map_err(other)?;
        Ok(Self {
            end,
            next_id: 0,
            handles: HashMap::new(),
            started: HashMap::new(),
            out: Vec::new(),
            inbuf: Vec::new(),
            _one_thread: PhantomData,
        })
    }

    /// Another sender, for another thread, asked of the node through this
    /// one.
    ///
    /// # Errors
    ///
    /// [`SenderError::LimitReached`] at the app's `senders_max`, or why
    /// the channel could not be made.
    pub fn try_clone(&mut self) -> Result<Self, SenderError> {
        match self.request(Request::OpenSender) {
            Answer::Sender(result) => Self::from_channel(result?),
            other => unexpected(&other),
        }
    }

    /// This instance's pid.
    pub fn self_pid(&mut self) -> Pid {
        match self.request(Request::SelfPid) {
            Answer::Pid(pid) => pid,
            other => unexpected(&other),
        }
    }

    /// Send `msg` to `to` without waiting; dropped if `to` is gone.
    pub fn cast(&mut self, to: Pid, msg: Envelope) {
        let _ = self.try_cast(to, msg);
    }

    /// [`Sender::cast`], saying whether the message was queued.
    ///
    /// # Errors
    ///
    /// The [`CastError`] saying why the message was dropped.
    pub fn try_cast(&mut self, to: Pid, msg: Envelope) -> Result<(), CastError> {
        match self.request(Request::Cast { to, msg }) {
            Answer::Unit => Ok(()),
            Answer::CastFailed(e) => Err(e),
            other => unexpected(&other),
        }
    }

    /// [`Sender::try_cast`] that waits up to `timeout_ms` for room if
    /// `to`'s mailbox is full. Only this sender waits: the callbacks and
    /// the app's other senders go on.
    ///
    /// # Errors
    ///
    /// The [`CastError`] saying why the message was not queued.
    pub fn cast_timeout(
        &mut self,
        to: Pid,
        msg: Envelope,
        timeout_ms: u32,
    ) -> Result<(), CastError> {
        match self.request(Request::CastWait {
            to,
            msg,
            timeout_ms,
        }) {
            Answer::Unit => Ok(()),
            Answer::CastFailed(e) => Err(e),
            other => unexpected(&other),
        }
    }

    /// Call `to` and wait up to `timeout_ms` for its reply. A call to this
    /// instance's own pid is answered by its callbacks, on the callback
    /// thread.
    ///
    /// # Errors
    ///
    /// The [`CallError`] if there is no reply.
    pub fn call(&mut self, to: Pid, msg: Envelope, timeout_ms: u32) -> Result<Envelope, CallError> {
        match self.request(Request::Call {
            to,
            msg,
            timeout_ms,
        }) {
            Answer::Called(result) => result,
            other => unexpected(&other),
        }
    }

    /// Call the app called `app` on this node, as
    /// [`Ctx::call_app`](crate::app::Ctx::call_app) does.
    ///
    /// # Errors
    ///
    /// The [`CallError`] if there is no reply.
    pub fn call_app(
        &mut self,
        app: impl Into<String>,
        msg: Envelope,
        timeout_ms: u32,
    ) -> Result<Envelope, CallError> {
        match self.request(Request::CallApp {
            app: app.into(),
            msg,
            timeout_ms,
        }) {
            Answer::Called(result) => result,
            other => unexpected(&other),
        }
    }

    /// Start a call to the app called `app` without waiting for its
    /// reply: its id, for [`Sender::next_answer`].
    pub fn start_call_app(
        &mut self,
        app: impl Into<String>,
        msg: Envelope,
        timeout_ms: u32,
    ) -> u64 {
        let id = self.send_request(Request::CallApp {
            app: app.into(),
            msg,
            timeout_ms,
        });
        self.started.insert(id, None);
        id
    }

    /// Start a call without waiting for its reply: its id, for
    /// [`Sender::next_answer`].
    pub fn start_call(&mut self, to: Pid, msg: Envelope, timeout_ms: u32) -> u64 {
        let id = self.send_request(Request::Call {
            to,
            msg,
            timeout_ms,
        });
        self.started.insert(id, None);
        id
    }

    /// Calls started and not yet answered by [`Sender::next_answer`].
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.started.len()
    }

    /// The next started call to be answered, in the order the answers
    /// come, waiting for one; `None` when no call is in flight.
    pub fn next_answer(&mut self) -> Option<(u64, Result<Envelope, CallError>)> {
        if self.started.is_empty() {
            return None;
        }
        let ready = self
            .started
            .iter()
            .find_map(|(id, answer)| answer.is_some().then_some(*id));
        if let Some(id) = ready {
            let answer = self.started.remove(&id).flatten()?;
            return Some((id, answer));
        }
        // Nothing else waits on this sender while it is here, so the next
        // frame answers a started call.
        match self.read() {
            Got::Frame(ToApp::Answer {
                id,
                answer: Answer::Called(result),
            }) if self.started.contains_key(&id) => {
                self.started.remove(&id);
                Some((id, result))
            }
            Got::Frame(other) => unexpected_frame(&other),
            Got::Hot => protocol_error("a hot frame with no request waiting"),
        }
    }

    /// One keyvalue request as the protocol has it. Watches are the
    /// callback thread's: a sender's `watch` is refused.
    ///
    /// # Errors
    ///
    /// The [`KvError`] the node answers.
    pub fn kv(&mut self, request: KvRequest) -> Result<KvAnswer, KvError> {
        let hot = match &request {
            KvRequest::Get { bucket, key } => Some((bucket, KvOp::Get, key, &[][..])),
            KvRequest::Set { bucket, key, value } => Some((bucket, KvOp::Set, key, &value.0[..])),
            KvRequest::Delete { bucket, key } => Some((bucket, KvOp::Delete, key, &[][..])),
            KvRequest::Exists { bucket, key } => Some((bucket, KvOp::Exists, key, &[][..])),
            _ => None,
        };
        if let Some((bucket, op, key, value)) = hot {
            return self.kv_op(bucket, op, key, value);
        }
        match self.request(Request::Kv(request)) {
            Answer::Kv(result) => result,
            other => unexpected(&other),
        }
    }

    /// A `get`, `set`, `delete` or `exists`: one hot frame on the
    /// bucket's handle (opened on first use), or the CBOR request for a key
    /// or value too long for the hot layout. Reads go to the node, not to a
    /// published bucket's snapshot.
    ///
    /// # Errors
    ///
    /// The [`KvError`] the node answers.
    pub fn kv_op(
        &mut self,
        bucket: &str,
        op: KvOp,
        key: &str,
        value: &[u8],
    ) -> Result<KvAnswer, KvError> {
        let handle = self.handle(bucket)?;
        self.next_id += 1;
        let id = self.next_id;
        let frame = KvFrame {
            id,
            bucket: handle,
            op,
            key,
            value,
        };
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        let written = crate::hot::write_kv_request(&mut out, &frame).is_some();
        if written {
            self.write(&out);
        }
        self.out = out;
        if !written {
            return match self.request(Request::Kv(cold_request(bucket, op, key, value))) {
                Answer::Kv(result) => result,
                other => unexpected(&other),
            };
        }
        loop {
            match self.read() {
                Got::Hot => {
                    return match crate::hot::parse_kv_answer(&self.inbuf) {
                        Ok((got, reply)) if got == id => Ok(match reply {
                            KvReply::Value(v) => KvAnswer::Value(v.map(Bytes::from)),
                            KvReply::Unit => KvAnswer::Unit,
                            KvReply::Exists(e) => KvAnswer::Exists(e),
                        }),
                        Ok((got, _)) => protocol_error(&format!("expected answer {id}, got {got}")),
                        Err(e) => protocol_error(&e.to_string()),
                    };
                }
                Got::Frame(ToApp::Answer {
                    id: got,
                    answer: Answer::Kv(result),
                }) if got == id => return result,
                Got::Frame(frame) => self.early(frame),
            }
        }
    }

    /// Register a metric (`bulkhead:metrics`) for this sender's updates:
    /// the same series as the callbacks' by name, with an id of this
    /// sender's own.
    ///
    /// # Errors
    ///
    /// As [`Ctx::register_metric`].
    pub fn register_metric(
        &mut self,
        kind: MetricKind,
        name: impl Into<String>,
        help: impl Into<String>,
        labels: &[(&str, &str)],
        buckets: &[f64],
    ) -> Result<u32, MetricError> {
        match self.request(Request::MetricRegister {
            kind,
            name: name.into(),
            help: help.into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            buckets: buckets.to_vec(),
        }) {
            Answer::Metric(result) => result,
            other => unexpected(&other),
        }
    }

    /// Add to a counter or gauge this sender registered. Sent at once, with
    /// no answer; the node logs one it refuses.
    pub fn metric_add(&mut self, metric: u32, delta: f64) {
        self.send(&FromApp::Metrics(vec![MetricUpdate::Add(metric, delta)]));
    }

    /// Set a gauge, as [`Sender::metric_add`] sends.
    pub fn metric_set(&mut self, metric: u32, value: f64) {
        self.send(&FromApp::Metrics(vec![MetricUpdate::Set(metric, value)]));
    }

    /// Record a histogram observation, as [`Sender::metric_add`] sends.
    pub fn metric_observe(&mut self, metric: u32, value: f64) {
        self.send(&FromApp::Metrics(vec![MetricUpdate::Observe(
            metric, value,
        )]));
    }

    /// Send `request` without waiting for an answer: for tests of what
    /// the node refuses on a sender channel only.
    #[doc(hidden)]
    pub fn send_for_tests(&mut self, request: Request) {
        self.send_request(request);
    }

    fn handle(&mut self, bucket: &str) -> Result<u32, KvError> {
        if let Some(h) = self.handles.get(bucket) {
            return Ok(*h);
        }
        match self.request(Request::KvOpen(bucket.to_string())) {
            Answer::KvOpened(Ok(h)) => {
                self.handles.insert(bucket.to_string(), h);
                Ok(h)
            }
            Answer::KvOpened(Err(e)) => Err(e),
            other => unexpected(&other),
        }
    }

    /// Ask and wait for the answer; answers to started calls that come
    /// first are kept for [`Sender::next_answer`].
    fn request(&mut self, request: Request) -> Answer {
        let id = self.send_request(request);
        loop {
            match self.read() {
                Got::Frame(ToApp::Answer { id: got, answer }) if got == id => return answer,
                Got::Frame(frame) => self.early(frame),
                Got::Hot => protocol_error("a hot frame with no hot request waiting"),
            }
        }
    }

    /// Keep a started call's answer that came while another request
    /// waited; anything else breaks the protocol.
    fn early(&mut self, frame: ToApp) {
        match frame {
            ToApp::Answer {
                id,
                answer: Answer::Called(result),
            } if self.started.contains_key(&id) => {
                self.started.insert(id, Some(result));
            }
            other => unexpected_frame(&other),
        }
    }

    fn send_request(&mut self, request: Request) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&FromApp::Request { id, request });
        id
    }

    fn send(&mut self, frame: &FromApp) {
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        if crate::hot::write_from_app(&mut out, frame).is_some() {
            self.write(&out);
        } else {
            self.write(&protocol::encode(frame));
        }
        self.out = out;
    }

    fn write(&self, bytes: &[u8]) {
        if ToNode(&self.end).write_all(bytes).is_err() {
            // The node broke the ring; it ends this process.
            gone();
        }
    }

    /// The next frame from the node; its end means the node stopped this
    /// instance, and the process exits.
    fn read(&mut self) -> Got {
        let mut prefix = [0u8; 4];
        match FromNode(&self.end).read_exact(&mut prefix) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => gone(),
            Err(e) => protocol_error(&e.to_string()),
        }
        let len = protocol::frame_len(prefix).unwrap_or_else(|e| protocol_error(&e.to_string()));
        self.inbuf.resize(len, 0);
        if let Err(e) = FromNode(&self.end).read_exact(&mut self.inbuf) {
            protocol_error(&e.to_string());
        }
        if crate::hot::is_hot(&self.inbuf) {
            return match crate::hot::parse_to_app(&self.inbuf) {
                Ok(crate::hot::ToAppHot::Kv(..)) => Got::Hot,
                Ok(crate::hot::ToAppHot::Frame(frame)) => Got::Frame(frame),
                Err(e) => protocol_error(&e.to_string()),
            };
        }
        match protocol::decode::<ToApp>(&self.inbuf) {
            Ok(frame) => Got::Frame(frame),
            Err(e) => protocol_error(&e.to_string()),
        }
    }
}

impl Ctx {
    /// A sender channel for one of the app's threads (ROADMAP AP1): hand
    /// it to the thread, which asks the node through it at any time.
    ///
    /// # Errors
    ///
    /// [`SenderError::LimitReached`] at the app's `senders_max`, or why
    /// the channel could not be made.
    pub fn sender(&mut self) -> Result<Sender, SenderError> {
        match crate::app::request(Request::OpenSender) {
            Answer::Sender(result) => Sender::from_channel(result?),
            other => unexpected(&other),
        }
    }
}

/// A hot keyvalue operation as the CBOR request it stands for.
fn cold_request(bucket: &str, op: KvOp, key: &str, value: &[u8]) -> KvRequest {
    let (bucket, key) = (bucket.to_string(), key.to_string());
    match op {
        KvOp::Get => KvRequest::Get { bucket, key },
        KvOp::Set => KvRequest::Set {
            bucket,
            key,
            value: Bytes::from(value),
        },
        KvOp::Delete => KvRequest::Delete { bucket, key },
        KvOp::Exists => KvRequest::Exists { bucket, key },
    }
}

fn unexpected(answer: &Answer) -> ! {
    protocol_error(&format!("unexpected answer {answer:?}"))
}

fn unexpected_frame(frame: &ToApp) -> ! {
    protocol_error(&format!("unexpected frame on a sender: {frame:?}"))
}

fn protocol_error(why: &str) -> ! {
    crate::app::protocol_error(&format!("sender: {why}"))
}

fn gone() -> ! {
    crate::app::gone()
}
