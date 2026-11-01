//! A native app as an executable: implement [`App`] and call [`serve`]
//! from `main`.
//!
//! ```no_run
//! use bulkhead_native::app::{App, CallRef, Ctx, serve};
//! use bulkhead_native::protocol::Envelope;
//!
//! struct Echo;
//!
//! impl App for Echo {
//!     fn init(_ctx: &mut Ctx, _args: Vec<u8>) -> Result<Self, String> {
//!         Ok(Echo)
//!     }
//!
//!     fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Option<Envelope> {
//!         Some(msg)
//!     }
//! }
//!
//! fn main() -> std::process::ExitCode {
//!     serve::<Echo>()
//! }
//! ```
//!
//! The node runs one callback at a time, as for a component. The process
//! functions are on [`Ctx`], valid during a callback; [`log`] works from
//! any thread at any time. Frames go over the stdout the node gave the
//! process, which [`serve`] moves to a descriptor of its own, pointing
//! stdout at stderr: what the app prints becomes log lines, never a
//! corrupt frame. The app must not read stdin.
//!
//! A panic in a callback crashes the instance with the panic's message.
//!
//! The thread that runs the callbacks also reads the node's frames: a
//! frame wakes it directly, with no hand-off from a reading thread (Z1,
//! spike S30). The node pings only while the instance is idle, so a ping
//! is answered between callbacks (or, if one arrives during a request,
//! there).

use std::cell::RefCell;
use std::fs::File;
use std::io::StdinLock;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::protocol::{
    self, Answer, Bytes, CallError, ConfigError, Envelope, ExitReason, FromApp, GroupsAnswer,
    GroupsError, GroupsRequest, Info, KvAnswer, KvError, KvRequest, Level, MetricError, MetricKind,
    MetricUpdate, Pid, RegisterError, Request, ScanPage, ScanQuery, SecretError, ToApp, VERSION,
};

/// A call's reference, for a later [`Ctx::reply`].
pub type CallRef = u64;

/// A native app's callbacks.
pub trait App: Sized {
    /// Start, from the app's init arguments (a supervisor child's
    /// `start_args` as JSON, or empty). An error ends the instance.
    ///
    /// # Errors
    ///
    /// Why the app cannot start.
    fn init(ctx: &mut Ctx, args: Vec<u8>) -> Result<Self, String>;

    /// Answer a call: `Some` replies now, `None` replies later with
    /// [`Ctx::reply`].
    fn handle_call(&mut self, ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Option<Envelope>;

    /// Handle a cast.
    fn handle_cast(&mut self, _ctx: &mut Ctx, _msg: Envelope) {}

    /// Handle an exit signal, a monitor's notice, a timer or a host event.
    fn handle_info(&mut self, _ctx: &mut Ctx, _info: Info) {}

    /// Stop gracefully.
    fn terminate(&mut self, _ctx: &mut Ctx, _reason: ExitReason) {}

    /// Write the state, for a snapshot upgrade, passivation or a move to
    /// another node, without changing it: if what follows fails, this
    /// instance keeps running. Empty by default.
    ///
    /// # Errors
    ///
    /// Why the state cannot be written: the instance crashes with it.
    fn save_snapshot(&self) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }

    /// Start from a snapshot written by version `from_version` of this
    /// app, in place of `init`. Refused by default.
    ///
    /// # Errors
    ///
    /// Why it is refused: an upgrade keeps the old version running; a
    /// wake from passivation or a move ends the instance.
    fn load_snapshot(
        _ctx: &mut Ctx,
        from_version: &str,
        _snapshot: Vec<u8>,
    ) -> Result<Self, String> {
        Err(format!(
            "this version does not load snapshots (from {from_version}); upgrade in restart mode"
        ))
    }
}

/// The descriptor frames go out on.
static OUT: OnceLock<Mutex<File>> = OnceLock::new();

/// Send one frame to the node. Without a node (before [`serve`], or once
/// the node has gone) a log record goes to stderr and anything else is
/// dropped.
fn send(frame: &FromApp) {
    let Some(out) = OUT.get() else {
        if let FromApp::Log { level, message } = frame {
            eprintln!("{level:?}: {message}");
        }
        return;
    };
    let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
    // A failed write means the node has gone; the reader sees the end of
    // stdin and the process exits.
    let _ = protocol::write(&mut *out, frame);
}

/// Log a record through the node's logging, from any thread.
pub fn log(level: Level, message: impl Into<String>) {
    send(&FromApp::Log {
        level,
        message: message.into(),
    });
}

/// The connection to the node, owned by the thread [`serve`] runs on:
/// the frames it reads, its requests' ids, and the callback's metric
/// updates.
struct Conn {
    input: StdinLock<'static>,
    next_id: u64,
    metrics: Vec<MetricUpdate>,
}

thread_local! {
    static CONN: RefCell<Option<Conn>> = const { RefCell::new(None) };
}

/// Run `f` on this thread's connection; on a thread without one (not the
/// one [`serve`] runs callbacks on) there is no node to ask, and the
/// process exits with a protocol error.
fn with_conn<R>(f: impl FnOnce(&mut Conn) -> R) -> R {
    CONN.with(|cell| match cell.borrow_mut().as_mut() {
        Some(conn) => f(conn),
        None => protocol_error("a request from a thread that does not run the callbacks"),
    })
}

/// The node's next frame, answering pings on the way; the end of stdin
/// means the node has gone, and the process exits.
fn next_frame() -> ToApp {
    with_conn(|conn| {
        loop {
            match protocol::read::<ToApp>(&mut conn.input) {
                Ok(Some(ToApp::Ping { id })) => send(&FromApp::Pong { id }),
                Ok(Some(frame)) => return frame,
                Ok(None) => gone(),
                Err(e) => protocol_error(&e.to_string()),
            }
        }
    })
}

/// Ask the node and wait for its answer. Anything else from the node here
/// breaks the protocol, and the end of stdin means the node has gone: the
/// process exits either way.
fn request(request: Request) -> Answer {
    let id = with_conn(|conn| {
        conn.next_id += 1;
        conn.next_id
    });
    send(&FromApp::Request { id, request });
    match next_frame() {
        ToApp::Answer { id: got, answer } if got == id => answer,
        other => protocol_error(&format!("expected answer {id}, got {other:?}")),
    }
}

/// Send the callback's metric updates, if any.
fn flush_metrics() {
    let updates = with_conn(|conn| std::mem::take(&mut conn.metrics));
    if !updates.is_empty() {
        send(&FromApp::Metrics(updates));
    }
}

/// The process functions and host services, during a callback. Every
/// `Ctx` is the same: [`Ctx::current`] gives one to code the callback does
/// not hand its own.
#[derive(Debug)]
pub struct Ctx {
    _private: (),
}

impl Ctx {
    /// The context of the callback running on this thread, for code not
    /// handed the callback's (an SDK's free functions). Its requests from
    /// any other thread end the process with a protocol error.
    #[must_use]
    pub fn current() -> Self {
        Self { _private: () }
    }

    fn request(&mut self, r: Request) -> Answer {
        request(r)
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
        self.request(Request::Cast { to, msg });
    }

    /// Call `to` and wait up to `timeout_ms` for its reply.
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

    /// Reply to a call taken with no reply.
    pub fn reply(&mut self, call_ref: CallRef, msg: Envelope) {
        self.request(Request::Reply { call_ref, msg });
    }

    /// Link to `other`: if either exits abnormally, the other gets an
    /// exit signal.
    pub fn link(&mut self, other: Pid) {
        self.request(Request::Link(other));
    }

    /// Remove a link.
    pub fn unlink(&mut self, other: Pid) {
        self.request(Request::Unlink(other));
    }

    /// Receive exit signals as [`Info::Exit`] instead of exiting.
    pub fn trap_exits(&mut self, on: bool) {
        self.request(Request::TrapExits(on));
    }

    /// Monitor `other`: its exit arrives as [`Info::Down`] with the
    /// returned reference.
    pub fn monitor(&mut self, other: Pid) -> u64 {
        match self.request(Request::Monitor(other)) {
            Answer::Ref(r) => r,
            other => unexpected(&other),
        }
    }

    /// Remove a monitor.
    pub fn demonitor(&mut self, reference: u64) {
        self.request(Request::Demonitor(reference));
    }

    /// Register `name` (one the manifest lists) for this instance.
    ///
    /// # Errors
    ///
    /// The [`RegisterError`] if the name is not listed or is taken.
    pub fn register(&mut self, name: impl Into<String>) -> Result<(), RegisterError> {
        match self.request(Request::Register(name.into())) {
            Answer::Registered(result) => result,
            other => unexpected(&other),
        }
    }

    /// The pid registered under `name`.
    pub fn whereis(&mut self, name: impl Into<String>) -> Option<Pid> {
        match self.request(Request::Whereis(name.into())) {
            Answer::MaybePid(pid) => pid,
            other => unexpected(&other),
        }
    }

    /// Deliver `msg` as [`Info::Timer`] after `ms`; returns the timer's
    /// reference.
    pub fn send_after(&mut self, ms: u32, msg: Envelope) -> u64 {
        match self.request(Request::SendAfter { ms, msg }) {
            Answer::Ref(r) => r,
            other => unexpected(&other),
        }
    }

    /// Cancel a timer.
    pub fn cancel_timer(&mut self, reference: u64) {
        self.request(Request::CancelTimer(reference));
    }

    /// Send an exit signal to `to`; to this instance's own pid, stop it.
    pub fn exit(&mut self, to: Pid, reason: ExitReason) {
        self.request(Request::Exit { to, reason });
    }

    /// A keyvalue bucket the manifest lists (the app's own, or a shared
    /// one its grant includes). Each operation is one request to the
    /// node, about 13 µs on a Raspberry Pi 5: read many keys with
    /// [`Bucket::get_many`] or [`Bucket::scan`] rather than one at a time.
    pub fn bucket(&mut self, name: impl Into<String>) -> Bucket<'_> {
        Bucket {
            ctx: self,
            name: name.into(),
        }
    }

    /// Join a process group the manifest lists for joining.
    ///
    /// # Errors
    ///
    /// [`GroupsError::AccessDenied`] if not granted or not listed;
    /// [`GroupsError::LimitReached`] over `limits.groups_max`.
    pub fn join_group(&mut self, group: impl Into<String>) -> Result<(), GroupsError> {
        self.groups(GroupsRequest::Join(group.into())).map(|_| ())
    }

    /// Leave a group.
    ///
    /// # Errors
    ///
    /// [`GroupsError::AccessDenied`] if not granted or not listed.
    pub fn leave_group(&mut self, group: impl Into<String>) -> Result<(), GroupsError> {
        self.groups(GroupsRequest::Leave(group.into())).map(|_| ())
    }

    /// A group's members across the cluster.
    ///
    /// # Errors
    ///
    /// [`GroupsError::AccessDenied`] for a group the manifest lists for
    /// neither joining nor publishing.
    pub fn group_members(&mut self, group: impl Into<String>) -> Result<Vec<Pid>, GroupsError> {
        match self.groups(GroupsRequest::Members(group.into()))? {
            GroupsAnswer::Members(pids) => Ok(pids),
            other => protocol_error(&format!("unexpected groups answer {other:?}")),
        }
    }

    /// Cast `msg` to every member of a group the manifest lists for
    /// publishing; how many members it was queued for.
    ///
    /// # Errors
    ///
    /// [`GroupsError::AccessDenied`] if not granted or not listed.
    pub fn publish(&mut self, group: impl Into<String>, msg: Envelope) -> Result<u32, GroupsError> {
        let group = group.into();
        match self.groups(GroupsRequest::Publish { group, msg })? {
            GroupsAnswer::Delivered(n) => Ok(n),
            other => protocol_error(&format!("unexpected groups answer {other:?}")),
        }
    }

    fn groups(&mut self, request: GroupsRequest) -> Result<GroupsAnswer, GroupsError> {
        match self.request(Request::Groups(request)) {
            Answer::Groups(result) => result,
            other => unexpected(&other),
        }
    }

    /// A configuration value (`wasi:config`: the manifest's `config` with
    /// the node's overrides), if set.
    ///
    /// # Errors
    ///
    /// [`ConfigError::AccessDenied`] if `wasi:config` is not granted.
    pub fn config(&mut self, key: impl Into<String>) -> Result<Option<String>, ConfigError> {
        match self.request(Request::ConfigGet(key.into())) {
            Answer::Config(result) => result,
            other => unexpected(&other),
        }
    }

    /// Every configuration value.
    ///
    /// # Errors
    ///
    /// As for [`Ctx::config`].
    pub fn config_all(&mut self) -> Result<Vec<(String, String)>, ConfigError> {
        match self.request(Request::ConfigAll) {
            Answer::ConfigAll(result) => result,
            other => unexpected(&other),
        }
    }

    /// A secret the manifest lists, read by the node when asked.
    ///
    /// # Errors
    ///
    /// [`SecretError::AccessDenied`] if not granted or not listed;
    /// [`SecretError::NotFound`] or [`SecretError::Unavailable`] if the
    /// node has no secret of that name or cannot read it.
    pub fn secret(&mut self, name: impl Into<String>) -> Result<Vec<u8>, SecretError> {
        match self.request(Request::Secret(name.into())) {
            Answer::Secret(result) => result.map(|b| b.0),
            other => unexpected(&other),
        }
    }

    /// Register a metric (`bulkhead:metrics`), or find the one already
    /// registered with this name and labels; its id for
    /// [`Ctx::metric_add`], [`Ctx::metric_set`] and [`Ctx::metric_observe`].
    /// Register in `init` and keep the id.
    ///
    /// # Errors
    ///
    /// [`MetricError::AccessDenied`] if not granted;
    /// [`MetricError::Invalid`] for a bad name, label or bucket;
    /// [`MetricError::LimitReached`] over the app's series limit.
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

    /// Add to a counter or gauge. Updates are kept and sent together just
    /// before the callback's result, in one frame with no answer, so they
    /// cost no request each; the node logs one it refuses (an unknown id,
    /// a counter going down).
    pub fn metric_add(&mut self, metric: u32, delta: f64) {
        with_conn(|conn| conn.metrics.push(MetricUpdate::Add(metric, delta)));
    }

    /// Set a gauge (sent as [`Ctx::metric_add`]'s updates are).
    pub fn metric_set(&mut self, metric: u32, value: f64) {
        with_conn(|conn| conn.metrics.push(MetricUpdate::Set(metric, value)));
    }

    /// Record a histogram observation (sent as [`Ctx::metric_add`]'s
    /// updates are).
    pub fn metric_observe(&mut self, metric: u32, value: f64) {
        with_conn(|conn| conn.metrics.push(MetricUpdate::Observe(metric, value)));
    }

    /// One keyvalue request as the protocol has it, for an SDK that wraps
    /// the operations itself; [`Ctx::bucket`] is the typed form.
    ///
    /// # Errors
    ///
    /// The [`KvError`] the node answers.
    pub fn kv(&mut self, request: KvRequest) -> Result<KvAnswer, KvError> {
        kv(self, request)
    }

    /// End a watch; notices for it still queued are dropped.
    ///
    /// # Errors
    ///
    /// None today; reserved.
    pub fn unwatch(&mut self, id: u64) -> Result<(), KvError> {
        kv(self, KvRequest::Unwatch(id)).map(|_| ())
    }
}

/// One keyvalue request.
fn kv(ctx: &mut Ctx, request: KvRequest) -> Result<KvAnswer, KvError> {
    match ctx.request(Request::Kv(request)) {
        Answer::Kv(result) => result,
        other => unexpected(&other),
    }
}

fn unexpected_kv(answer: &KvAnswer) -> ! {
    protocol_error(&format!("unexpected keyvalue answer {answer:?}"))
}

/// A bucket, from [`Ctx::bucket`]: `wasi:keyvalue`'s operations and
/// `bulkhead:watch`'s (revisions, compare-and-swap, scans, watches).
///
/// Every method fails with [`KvError::AccessDenied`] for a bucket the
/// manifest does not list or the grant does not include, and for a write
/// to a read-only shared bucket; [`KvError::NoSuchBucket`] for a shared
/// bucket the node does not configure.
#[derive(Debug)]
pub struct Bucket<'a> {
    ctx: &'a mut Ctx,
    name: String,
}

/// A started watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watched {
    /// Its id, for [`Ctx::unwatch`] and in its notices.
    pub id: u64,
    /// The bucket's revision when it started.
    pub revision: u64,
}

impl Bucket<'_> {
    fn kv(&mut self, request: KvRequest) -> Result<KvAnswer, KvError> {
        kv(self.ctx, request)
    }

    fn bucket(&self) -> String {
        self.name.clone()
    }

    /// A key's value.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        let (bucket, key) = (self.bucket(), key.to_string());
        match self.kv(KvRequest::Get { bucket, key })? {
            KvAnswer::Value(v) => Ok(v.map(|b| b.0)),
            other => unexpected_kv(&other),
        }
    }

    /// Set a key.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`]; [`KvError::Other`] over the bucket's quota.
    pub fn set(&mut self, key: &str, value: &[u8]) -> Result<(), KvError> {
        let (bucket, key) = (self.bucket(), key.to_string());
        let value = Bytes::from(value);
        self.kv(KvRequest::Set { bucket, key, value }).map(|_| ())
    }

    /// Delete a key; deleting a missing key succeeds.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn delete(&mut self, key: &str) -> Result<(), KvError> {
        let (bucket, key) = (self.bucket(), key.to_string());
        self.kv(KvRequest::Delete { bucket, key }).map(|_| ())
    }

    /// Whether a key exists.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn exists(&mut self, key: &str) -> Result<bool, KvError> {
        let (bucket, key) = (self.bucket(), key.to_string());
        match self.kv(KvRequest::Exists { bucket, key })? {
            KvAnswer::Exists(e) => Ok(e),
            other => unexpected_kv(&other),
        }
    }

    /// A page of the bucket's keys from `cursor` (`None`: the first), and
    /// the next page's cursor.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn keys(&mut self, cursor: Option<u64>) -> Result<(Vec<String>, Option<u64>), KvError> {
        let bucket = self.bucket();
        match self.kv(KvRequest::Keys { bucket, cursor })? {
            KvAnswer::Keys(keys, next) => Ok((keys, next)),
            other => unexpected_kv(&other),
        }
    }

    /// Add `delta` to a counter (missing: 0); its new value.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`]; [`KvError::Other`] if the value is not a
    /// counter.
    pub fn increment(&mut self, key: &str, delta: u64) -> Result<u64, KvError> {
        let (bucket, key) = (self.bucket(), key.to_string());
        match self.kv(KvRequest::Increment { bucket, key, delta })? {
            KvAnswer::Count(n) => Ok(n),
            other => unexpected_kv(&other),
        }
    }

    /// Several keys' values in one request, in the order given.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn get_many(&mut self, keys: &[String]) -> Result<Vec<Option<Vec<u8>>>, KvError> {
        let (bucket, keys) = (self.bucket(), keys.to_vec());
        match self.kv(KvRequest::GetMany { bucket, keys })? {
            KvAnswer::Values(found) => Ok(found.into_iter().map(|v| v.map(|b| b.0)).collect()),
            other => unexpected_kv(&other),
        }
    }

    /// Set several keys as one change.
    ///
    /// # Errors
    ///
    /// As for [`Bucket::set`]; nothing is written if any fails.
    pub fn set_many(&mut self, entries: &[(String, Vec<u8>)]) -> Result<(), KvError> {
        let bucket = self.bucket();
        let entries = entries
            .iter()
            .map(|(k, v)| (k.clone(), Bytes(v.clone())))
            .collect();
        self.kv(KvRequest::SetMany { bucket, entries }).map(|_| ())
    }

    /// Delete several keys as one change.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn delete_many(&mut self, keys: &[String]) -> Result<(), KvError> {
        let (bucket, keys) = (self.bucket(), keys.to_vec());
        self.kv(KvRequest::DeleteMany { bucket, keys }).map(|_| ())
    }

    fn entry_of(
        &mut self,
        key: &str,
        linearizable: bool,
    ) -> Result<Option<(Vec<u8>, u64)>, KvError> {
        let (bucket, key) = (self.bucket(), key.to_string());
        match self.kv(KvRequest::Entry {
            bucket,
            key,
            linearizable,
        })? {
            KvAnswer::Entry(e) => Ok(e.map(|(v, r)| (v.0, r))),
            other => unexpected_kv(&other),
        }
    }

    /// A key's value and the revision it was written at.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn entry(&mut self, key: &str) -> Result<Option<(Vec<u8>, u64)>, KvError> {
        self.entry_of(key, false)
    }

    /// As [`Bucket::entry`], served by a replicated bucket's primary, so it
    /// reflects every write committed before it.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`]; [`KvError::Unavailable`] with no primary.
    pub fn entry_linearizable(&mut self, key: &str) -> Result<Option<(Vec<u8>, u64)>, KvError> {
        self.entry_of(key, true)
    }

    /// The bucket's revision (0 before its first change).
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn revision(&mut self) -> Result<u64, KvError> {
        let bucket = self.bucket();
        match self.kv(KvRequest::Revision { bucket })? {
            KvAnswer::Revision(r) => Ok(r),
            other => unexpected_kv(&other),
        }
    }

    /// One page of the bucket's entries in key order.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn scan(&mut self, query: ScanQuery) -> Result<ScanPage, KvError> {
        let bucket = self.bucket();
        match self.kv(KvRequest::Scan { bucket, query })? {
            KvAnswer::Page(page) => Ok(page),
            other => unexpected_kv(&other),
        }
    }

    /// Set a key if its revision is `expected` (`None`: only if absent);
    /// its new revision.
    ///
    /// # Errors
    ///
    /// [`KvError::Conflict`] with the key's revision if it differs; as for
    /// [`Bucket::set`] otherwise.
    pub fn compare_and_swap(
        &mut self,
        key: &str,
        expected: Option<u64>,
        value: &[u8],
    ) -> Result<u64, KvError> {
        let (bucket, key) = (self.bucket(), key.to_string());
        let value = Bytes::from(value);
        match self.kv(KvRequest::CompareAndSwap {
            bucket,
            key,
            expected,
            value,
        })? {
            KvAnswer::Revision(r) => Ok(r),
            other => unexpected_kv(&other),
        }
    }

    /// Delete a key if its revision is `expected`; the bucket's new
    /// revision.
    ///
    /// # Errors
    ///
    /// [`KvError::Conflict`] with the key's revision if it differs.
    pub fn compare_and_delete(&mut self, key: &str, expected: u64) -> Result<u64, KvError> {
        let (bucket, key) = (self.bucket(), key.to_string());
        match self.kv(KvRequest::CompareAndDelete {
            bucket,
            key,
            expected,
        })? {
            KvAnswer::Revision(r) => Ok(r),
            other => unexpected_kv(&other),
        }
    }

    fn start(&mut self, key: &str, prefix: bool, after: Option<u64>) -> Result<Watched, KvError> {
        let (bucket, key) = (self.bucket(), key.to_string());
        match self.kv(KvRequest::Watch {
            bucket,
            key,
            prefix,
            after,
        })? {
            KvAnswer::Started { id, revision } => Ok(Watched { id, revision }),
            other => unexpected_kv(&other),
        }
    }

    /// Watch a key from now: each change arrives in `handle_info` as
    /// [`Info::KvChanged`], with the
    /// key's value.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`]; [`KvError::LimitReached`] over
    /// `limits.watches_max`.
    pub fn watch(&mut self, key: &str) -> Result<Watched, KvError> {
        self.start(key, false, None)
    }

    /// Watch every key starting with `prefix` (all of them for `""`), from
    /// now. To keep a copy of the keys, watch first and then
    /// [`scan`](Bucket::scan): a change during the scan arrives as a
    /// notice (spike S28).
    ///
    /// # Errors
    ///
    /// As for [`Bucket::watch`].
    pub fn watch_prefix(&mut self, prefix: &str) -> Result<Watched, KvError> {
        self.start(prefix, true, None)
    }

    /// Watch a key (or prefix), first noticing every matching key changed
    /// after revision `after`.
    ///
    /// # Errors
    ///
    /// [`KvError::TooOld`] if the bucket cannot replay from `after` (any
    /// key in it was deleted since); as for [`Bucket::watch`] otherwise.
    pub fn resume(&mut self, key: &str, prefix: bool, after: u64) -> Result<Watched, KvError> {
        self.start(key, prefix, Some(after))
    }
}

fn unexpected(answer: &Answer) -> ! {
    protocol_error(&format!("unexpected answer {answer:?}"))
}

fn protocol_error(what: &str) -> ! {
    eprintln!("bulkhead-native: protocol error: {what}");
    std::process::exit(3)
}

/// The node closed stdin: it has gone, or stopped this instance.
fn gone() -> ! {
    std::process::exit(0)
}

/// Run a callback, turning a panic into [`FromApp::Failed`] and an exit.
fn guarded<R>(f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(panic) => {
            let why = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".to_string());
            send(&FromApp::Failed(format!("panic: {why}")));
            std::process::exit(101)
        }
    }
}

/// Take stdout for frames, pointing stdout itself at stderr.
fn take_stdout() -> io::Result<File> {
    let stdout = io::stdout();
    let mut locked = stdout.lock();
    locked.flush()?;
    let frames = rustix::io::dup(locked.as_fd())?;
    rustix::stdio::dup2_stdout(io::stderr().as_fd())?;
    Ok(File::from(frames))
}

/// Serve `A` to the node over stdin and stdout until the node stops it.
#[must_use]
pub fn serve<A: App>() -> ExitCode {
    // Not dumpable again: the runner made itself so, but `execve` resets
    // it for a readable program. Without this a crash went through the
    // system's core handler (the node saw the exit 0.1 to 0.3 s late,
    // spike S30) and other processes of the node's user could trace the
    // app.
    #[cfg(target_os = "linux")]
    if let Err(e) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        eprintln!("bulkhead-native: not dumpable: {e}");
    }
    match take_stdout() {
        Ok(out) => {
            let _ = OUT.set(Mutex::new(out));
        }
        Err(e) => {
            eprintln!("bulkhead-native: stdout: {e}");
            return ExitCode::from(2);
        }
    }
    send(&FromApp::Hello { version: VERSION });
    CONN.with(|cell| {
        *cell.borrow_mut() = Some(Conn {
            input: io::stdin().lock(),
            next_id: 0,
            metrics: Vec::new(),
        });
    });
    let mut ctx = Ctx::current();
    let started = match next_frame() {
        ToApp::Init { args } => guarded(|| A::init(&mut ctx, args)),
        ToApp::LoadSnapshot {
            from_version,
            snapshot,
        } => guarded(|| A::load_snapshot(&mut ctx, &from_version, snapshot)),
        other => protocol_error(&format!("expected init, got {other:?}")),
    };
    let mut app = match started {
        Ok(app) => {
            flush_metrics();
            send(&FromApp::Init(Ok(())));
            app
        }
        Err(why) => {
            send(&FromApp::Init(Err(why)));
            return ExitCode::from(1);
        }
    };
    loop {
        match next_frame() {
            ToApp::Call { call_ref, msg } => {
                let reply = guarded(|| app.handle_call(&mut ctx, call_ref, msg));
                flush_metrics();
                send(&FromApp::Reply(reply));
            }
            ToApp::Cast { msg } => {
                guarded(|| app.handle_cast(&mut ctx, msg));
                flush_metrics();
                send(&FromApp::Done);
            }
            ToApp::Info { info } => {
                guarded(|| app.handle_info(&mut ctx, info));
                flush_metrics();
                send(&FromApp::Done);
            }
            ToApp::SaveSnapshot => match guarded(|| app.save_snapshot()) {
                Ok(bytes) => send(&FromApp::Snapshot(bytes.into())),
                Err(why) => {
                    send(&FromApp::Failed(format!("save_snapshot failed: {why}")));
                    return ExitCode::from(101);
                }
            },
            ToApp::Terminate { reason } => {
                guarded(|| app.terminate(&mut ctx, reason));
                flush_metrics();
                send(&FromApp::Done);
                return ExitCode::SUCCESS;
            }
            other => protocol_error(&format!("unexpected {other:?} between callbacks")),
        }
    }
}
