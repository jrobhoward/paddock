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
//! The node runs one callback at a time. The process
//! functions are on [`Ctx`], valid during a callback; [`log`] works from
//! any thread at any time. Frames go over the channel the node gave the
//! process (`crate::channel`, Linux only): two rings in shared memory,
//! each waiting side spinning briefly before it sleeps. What the app
//! prints, on stdout or stderr, becomes log lines; stdin is empty.
//!
//! A panic in a callback crashes the instance with the panic's message.
//!
//! The thread that runs the callbacks also reads the node's frames: a
//! frame wakes it directly, with no hand-off from a reading thread (Z1,
//! spike S30). The node pings only while the instance is idle, so a ping
//! is answered between callbacks (or, if one arrives during a request,
//! there).

use std::cell::RefCell;
#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock, PoisonError};

#[cfg(target_os = "linux")]
use crate::channel::{AppEnd, FromNode, ToNode};

use crate::hot::KvOp;
use crate::protocol::{
    self, Answer, Bytes, CallError, CastError, ConfigError, Envelope, ExitReason, ExtensionError,
    FdRef, FromApp, GroupsAnswer, GroupsError, GroupsRequest, HttpError, HttpRequest, HttpResponse,
    Info, KvAnswer, KvError, KvRequest, Level, MetricError, MetricKind, MetricUpdate, NetAnswer,
    NetError, NetRequest, NetStream, Pid, RegisterError, Request, ScanPage, ScanQuery, SecretError,
    SqlAnswer, SqlError, SqlRequest, ToApp, VERSION,
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

    /// Take a connection accepted on the listener `listener` (an app
    /// declaring `listeners`): take its descriptor with [`take_fd`], ask
    /// for readiness with [`NetRequest::Notify`] and close it with
    /// [`NetRequest::Close`], as a stream from [`NetRequest::Connect`].
    /// Closes it by default.
    fn handle_connection(&mut self, ctx: &mut Ctx, listener: String, stream: NetStream) {
        let _ = listener;
        drop(take_fd(stream.fd));
        let _ = ctx.net(NetRequest::Close(stream.socket));
    }

    /// Stop gracefully.
    fn terminate(&mut self, _ctx: &mut Ctx, _reason: ExitReason) {}

    /// Write the state, for a snapshot upgrade or a move to another
    /// node, without changing it: if what follows fails, this
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
    /// move ends the instance.
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

/// The channel to the node, once [`serve`] has opened it.
#[cfg(target_os = "linux")]
static END: OnceLock<AppEnd> = OnceLock::new();

/// One frame at a time to the node, whichever thread sends it.
#[cfg(target_os = "linux")]
static SENDING: Mutex<()> = Mutex::new(());

#[cfg(target_os = "linux")]
thread_local! {
    /// A hot frame being sent from this thread, kept between frames.
    static HOT_OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Send an encoded frame to the node, as [`send`] does.
#[cfg(target_os = "linux")]
fn send_bytes(frame: &[u8]) {
    use std::io::Write;

    if let Some(end) = END.get() {
        let _one = SENDING.lock().unwrap_or_else(PoisonError::into_inner);
        // A broken ring means the node broke it; it ends this process.
        let _ = ToNode(end).write_all(frame);
    }
}

/// Send one frame to the node. Without a node (before [`serve`]) a log
/// record goes to stderr and anything else is dropped.
fn send(frame: &FromApp) {
    #[cfg(target_os = "linux")]
    if END.get().is_some() {
        // A hot frame where the layouts cover it (ROADMAP T7), written into
        // this thread's buffer; CBOR otherwise.
        let hot = HOT_OUT.with(|out| {
            let mut out = out.borrow_mut();
            out.clear();
            crate::hot::write_from_app(&mut out, frame)?;
            send_bytes(&out);
            if out.capacity() > KEPT_BUFFER {
                *out = Vec::new();
            }
            Some(())
        });
        if hot.is_none() {
            send_bytes(&protocol::encode(frame));
        }
        return;
    }
    if let FromApp::Log { level, message } = frame {
        eprintln!("{level:?}: {message}");
    }
}

/// Log a record through the node's logging, from any thread.
pub fn log(level: Level, message: impl Into<String>) {
    send(&FromApp::Log {
        level,
        message: message.into(),
    });
}

/// The connection to the node, owned by the thread [`serve`] runs on: its
/// requests' ids and the callback's metric updates. It alone reads the
/// node's frames.
struct Conn {
    next_id: u64,
    metrics: Vec<MetricUpdate>,
    /// Published buckets' snapshots, by bucket (ROADMAP T4).
    #[cfg(target_os = "linux")]
    published: HashMap<String, Published>,
    /// Published buckets this instance wrote, with the bucket's revision
    /// after its last write: its reads go to the node until a snapshot
    /// reaches that revision, so it reads its own writes.
    #[cfg(target_os = "linux")]
    written: HashMap<String, u64>,
    /// Bucket handles the node gave (ROADMAP T7), by bucket name.
    #[cfg(target_os = "linux")]
    handles: HashMap<String, u32>,
    /// The last frame read from the node, in `inbuf[..in_len]`; kept
    /// between frames so reading one allocates nothing.
    #[cfg(target_os = "linux")]
    inbuf: Vec<u8>,
    #[cfg(target_os = "linux")]
    in_len: usize,
    /// A hot frame being written, kept between frames.
    #[cfg(target_os = "linux")]
    out: Vec<u8>,
}

/// A published bucket's snapshot, mapped.
#[cfg(target_os = "linux")]
struct Published {
    mapped: crate::channel::Mapped,
    revision: u64,
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

/// The node's next frame, answering pings on the way; the end of the
/// channel means the node stopped this instance, and the process exits.
/// Tell the channel whether the app waits for its next callback, so a
/// callback the node hinted idle is followed by sleep, not a spin.
fn between_callbacks(on: bool) {
    #[cfg(target_os = "linux")]
    if let Some(end) = END.get() {
        end.set_between(on);
    }
    let _ = on;
}

fn next_frame() -> ToApp {
    match next() {
        Next::Frame(frame) => frame,
        #[cfg(target_os = "linux")]
        Next::Hot => protocol_error("a hot frame where none was asked for"),
    }
}

/// What [`next`] read.
// Read only from a node's channel, on Linux: elsewhere `next` has none.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Next {
    /// A CBOR frame.
    Frame(ToApp),
    /// A hot frame, left in the connection's `inbuf[..in_len]`.
    #[cfg(target_os = "linux")]
    Hot,
}

/// The node's next frame, answering pings and adopting snapshots on the
/// way.
fn next() -> Next {
    with_conn(|_| ());
    #[cfg(target_os = "linux")]
    if let Some(end) = END.get() {
        loop {
            match with_conn(|conn| read_frame(end, conn)) {
                Ok(true) => {}
                Ok(false) => gone(),
                Err(e) => protocol_error(&e.to_string()),
            }
            let hot = with_conn(|conn| {
                let body = &conn.inbuf[..conn.in_len];
                crate::hot::is_hot(body).then(|| match crate::hot::parse_to_app(body) {
                    Ok(crate::hot::ToAppHot::Kv(..)) => Ok(None),
                    Ok(crate::hot::ToAppHot::Frame(frame)) => Ok(Some(frame)),
                    Err(e) => Err(e),
                })
            });
            match hot {
                Some(Ok(None)) => return Next::Hot,
                Some(Ok(Some(frame))) => return Next::Frame(frame),
                Some(Err(e)) => protocol_error(&e.to_string()),
                None => {}
            }
            match with_conn(|conn| protocol::decode::<ToApp>(&conn.inbuf[..conn.in_len])) {
                Ok(ToApp::Ping { id }) => send(&FromApp::Pong { id }),
                Ok(ToApp::Published {
                    bucket,
                    revision,
                    fd,
                }) => adopt(bucket, revision, fd),
                Ok(frame) => return Next::Frame(frame),
                Err(e) => protocol_error(&e.to_string()),
            }
        }
    }
    protocol_error("no channel to the node")
}

/// Read one frame's body into `conn.inbuf[..conn.in_len]`, growing the
/// buffer only past the longest frame so far; `false` at the end of the
/// channel.
#[cfg(target_os = "linux")]
fn read_frame(end: &AppEnd, conn: &mut Conn) -> Result<bool, protocol::FrameError> {
    use std::io::Read;

    use protocol::FrameError;

    let mut prefix = [0u8; 4];
    let mut from = FromNode(end);
    match from.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(FrameError::Io(e)),
    }
    let len = protocol::frame_len(prefix)?;
    if conn.inbuf.len() > KEPT_BUFFER && len <= KEPT_BUFFER {
        // A large frame's buffer is not kept for the instance's life.
        conn.inbuf = Vec::new();
    }
    if conn.inbuf.len() < len {
        conn.inbuf.resize(len, 0);
    }
    from.read_exact(&mut conn.inbuf[..len])
        .map_err(FrameError::Io)?;
    conn.in_len = len;
    Ok(true)
}

/// Map a published bucket's new snapshot in place of the one before. One
/// that does not map leaves the bucket read from the node: slower, never
/// stale.
#[cfg(target_os = "linux")]
fn adopt(bucket: String, revision: u64, fd: FdRef) {
    let mapped = take_fd(fd)
        .and_then(|fd| crate::channel::Mapped::open(&fd))
        .and_then(|m| match crate::snapshot::Snapshot::parse(m.bytes()) {
            Ok(_) => Ok(m),
            Err(why) => Err(io::Error::new(io::ErrorKind::InvalidData, why)),
        });
    let failed = with_conn(|conn| match mapped {
        Ok(mapped) => {
            if conn.written.get(&bucket).is_some_and(|w| revision >= *w) {
                conn.written.remove(&bucket);
            }
            conn.published
                .insert(bucket, Published { mapped, revision });
            None
        }
        Err(e) => {
            conn.published.remove(&bucket);
            Some(format!(
                "published bucket {bucket}: {e}; reading it from the node"
            ))
        }
    });
    if let Some(why) = failed {
        log(Level::Warn, why);
    }
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

/// Roll back the transaction `tx` from a `Drop`: sent only from the
/// thread that runs the callbacks and not while it panics; otherwise left
/// for the node, which rolls it back when the instance exits. A failure is
/// ignored: the transaction has ended either way.
pub fn end_transaction_quietly(tx: u32) {
    request_quietly(Request::Sql(SqlRequest::TxEnd { tx, commit: false }));
}

/// Close the socket `socket` from a `Drop`, giving back its share of
/// `max_sockets`: sent as [`end_transaction_quietly`] sends; otherwise the
/// node keeps counting it until the instance exits.
pub fn close_socket_quietly(socket: u32) {
    request_quietly(Request::Net(NetRequest::Close(socket)));
}

/// Send `r` if this thread runs the callbacks and is not panicking,
/// ignoring the answer.
fn request_quietly(r: Request) {
    let here = CONN.with(|cell| cell.try_borrow().is_ok_and(|c| c.is_some()));
    if here && !std::thread::panicking() {
        let _ = request(r);
    }
}

/// Read what a passed stream has already received into `buf`, without
/// waiting (`MSG_DONTWAIT`): the stream stays blocking for writes.
///
/// # Errors
///
/// `WouldBlock` when nothing has arrived; otherwise the socket's error.
pub fn recv_dontwait(fd: std::os::fd::BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<usize> {
    rustix::net::recv(fd, buf, rustix::net::RecvFlags::DONTWAIT)
        .map(|(n, _)| n)
        .map_err(io::Error::from)
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

    /// [`Ctx::cast`], saying whether the message was queued: in the
    /// target's mailbox, or for an instance on another node, on the link
    /// to that node. It costs what a cast does.
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

    /// [`Ctx::try_cast`] that waits up to `timeout_ms` for room if `to`'s
    /// mailbox is full, so `Ok` means the message is in its mailbox. This
    /// callback handles nothing else meanwhile; the wait is host time,
    /// capped at the instance's callback deadline. A cast to this
    /// instance's own pid never waits.
    ///
    /// # Errors
    ///
    /// The [`CastError`] saying why the message was not queued:
    /// [`CastError::Timeout`] if no room came in time.
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
    /// node, about 2 to 3.5 µs for a `get`, `set` or `delete` on a 16-core
    /// machine (2026-11-04): read many keys with [`Bucket::get_many`] or
    /// [`Bucket::scan`] rather than one at a time.
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

    /// A configuration value (`bulkhead:config`: the manifest's `config` with
    /// the node's overrides), if set.
    ///
    /// # Errors
    ///
    /// [`ConfigError::AccessDenied`] if `bulkhead:config` is not granted.
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

    /// A socket operation (`bulkhead:sockets`, ROADMAP Z8e). A stream comes
    /// connected, on the descriptor socket ([`take_fd`]); listeners and
    /// UDP sockets stay in the node. [`close_socket_quietly`] closes one
    /// from a `Drop`.
    ///
    /// # Errors
    ///
    /// [`NetError::Denied`] if not granted or outside the manifest's
    /// rules, [`NetError::Limited`] over `max_sockets`, the others as the
    /// socket reported them.
    pub fn net(&mut self, request: NetRequest) -> Result<NetAnswer, NetError> {
        match self.request(Request::Net(request)) {
            Answer::Net(result) => result,
            other => unexpected(&other),
        }
    }

    /// An HTTP request (`bulkhead:http-egress`, ROADMAP Z8f), sent
    /// by the node's egress client on the manifest's rules; the whole
    /// response, up to a frame's size.
    ///
    /// # Errors
    ///
    /// [`HttpError::Denied`] if not granted or outside the `allow` list or
    /// methods, [`HttpError::Limited`] over the app's requests in flight,
    /// the others as the request failed.
    pub fn http(&mut self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        match self.request(Request::Http(request)) {
            Answer::Http(result) => result,
            other => unexpected(&other),
        }
    }

    /// Call `function` of the extension serving `package` (ROADMAP Z8h)
    /// with `args` in CBOR; its result in CBOR. The extension checks the
    /// app's grant and scope and puts its own errors in the result.
    ///
    /// # Errors
    ///
    /// [`ExtensionError::NoSuchExtension`] if the node runs none for
    /// `package`, and the others if the extension has no such function or
    /// cannot read `args`.
    pub fn extension(
        &mut self,
        package: impl Into<String>,
        function: impl Into<String>,
        args: Vec<u8>,
    ) -> Result<Vec<u8>, ExtensionError> {
        let asked = Request::Extension {
            package: package.into(),
            function: function.into(),
            args: Bytes(args),
        };
        match self.request(asked) {
            Answer::Extension(result) => result.map(|b| b.0),
            other => unexpected(&other),
        }
    }

    /// A SQL operation on a datasource the manifest lists
    /// (`bulkhead:sql`). A transaction ([`SqlRequest::Begin`]) holds one
    /// of the app's connections until [`SqlRequest::TxEnd`], or until the
    /// instance exits, which rolls it back; [`end_transaction_quietly`]
    /// rolls one back from a `Drop`.
    ///
    /// # Errors
    ///
    /// [`SqlError::AccessDenied`] if not granted or not listed; the others
    /// as `bulkhead:sql`'s `error`.
    pub fn sql(&mut self, request: SqlRequest) -> Result<SqlAnswer, SqlError> {
        match self.request(Request::Sql(request)) {
            Answer::Sql(result) => result,
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

    /// A `get`, `set`, `delete` or `exists` on a borrowed bucket name and
    /// key, for an SDK that wraps the operations itself: as [`Ctx::kv`]
    /// with the matching [`KvRequest`], without building it. `value` is
    /// the value to set, ignored by the other operations. The answer is
    /// [`KvAnswer::Value`], [`KvAnswer::Unit`] or [`KvAnswer::Exists`].
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
        kv_op(self, bucket, op, key, value)
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

/// The largest frame buffer kept between frames; a larger one is
/// released once a smaller frame follows.
#[cfg(target_os = "linux")]
const KEPT_BUFFER: usize = 1 << 20;

/// One keyvalue request.
fn kv(ctx: &mut Ctx, request: KvRequest) -> Result<KvAnswer, KvError> {
    let hot = match &request {
        KvRequest::Get { bucket, key } => Some((bucket, KvOp::Get, key, &[][..])),
        KvRequest::Set { bucket, key, value } => Some((bucket, KvOp::Set, key, &value.0[..])),
        KvRequest::Delete { bucket, key } => Some((bucket, KvOp::Delete, key, &[][..])),
        KvRequest::Exists { bucket, key } => Some((bucket, KvOp::Exists, key, &[][..])),
        _ => None,
    };
    if let Some((bucket, op, key, value)) = hot {
        return kv_op(ctx, bucket, op, key, value);
    }
    kv_cold(ctx, request)
}

/// One of the hot frames' keyvalue operations, by bucket name: read from a
/// published snapshot when it can be, otherwise one hot frame to the node
/// on the bucket's handle (opened on first use), or CBOR for a key or value
/// too long for the hot layout.
fn kv_op(
    ctx: &mut Ctx,
    bucket: &str,
    op: KvOp,
    key: &str,
    value: &[u8],
) -> Result<KvAnswer, KvError> {
    #[cfg(target_os = "linux")]
    {
        if let Some(answer) = published_key_read(bucket, op, key) {
            return Ok(answer);
        }
        let result = hot_request(ctx, bucket, op, key, value);
        if op != KvOp::Get
            && op != KvOp::Exists
            && result.is_ok()
            && with_conn(|conn| conn.published.contains_key(bucket))
        {
            note_write(ctx, bucket.to_string());
        }
        result
    }
    #[cfg(not(target_os = "linux"))]
    kv_cold(ctx, cold_request(bucket, op, key, value))
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

/// The bucket's handle, asked of the node the first time; a bucket that
/// does not open is asked again next time (its error is the operation's).
#[cfg(target_os = "linux")]
fn handle(ctx: &mut Ctx, bucket: &str) -> Result<u32, KvError> {
    if let Some(h) = with_conn(|conn| conn.handles.get(bucket).copied()) {
        return Ok(h);
    }
    match ctx.request(Request::KvOpen(bucket.to_string())) {
        Answer::KvOpened(Ok(h)) => {
            with_conn(|conn| conn.handles.insert(bucket.to_string(), h));
            Ok(h)
        }
        Answer::KvOpened(Err(e)) => Err(e),
        other => unexpected(&other),
    }
}

/// One hot keyvalue request and its answer: the frame written into the
/// connection's buffer, the answer read from it, a value copied out once.
#[cfg(target_os = "linux")]
fn hot_request(
    ctx: &mut Ctx,
    bucket: &str,
    op: KvOp,
    key: &str,
    value: &[u8],
) -> Result<KvAnswer, KvError> {
    use crate::hot::{KvFrame, KvReply};

    let handle = handle(ctx, bucket)?;
    let id = with_conn(|conn| {
        conn.next_id += 1;
        conn.next_id
    });
    let frame = KvFrame {
        id,
        bucket: handle,
        op,
        key,
        value,
    };
    let written = with_conn(|conn| {
        conn.out.clear();
        crate::hot::write_kv_request(&mut conn.out, &frame)?;
        if conn.out.capacity() > KEPT_BUFFER {
            // As for the frames read: not kept for the instance's life.
            send_bytes(&std::mem::take(&mut conn.out));
        } else {
            send_bytes(&conn.out);
        }
        Some(())
    });
    if written.is_none() {
        return kv_cold(ctx, cold_request(bucket, op, key, value));
    }
    match next() {
        Next::Hot => {
            with_conn(
                |conn| match crate::hot::parse_kv_answer(&conn.inbuf[..conn.in_len]) {
                    Ok((got, reply)) if got == id => Ok(match reply {
                        KvReply::Value(v) => KvAnswer::Value(v.map(Bytes::from)),
                        KvReply::Unit => KvAnswer::Unit,
                        KvReply::Exists(e) => KvAnswer::Exists(e),
                    }),
                    Ok((got, _)) => protocol_error(&format!("expected answer {id}, got {got}")),
                    Err(e) => protocol_error(&e.to_string()),
                },
            )
        }
        Next::Frame(ToApp::Answer {
            id: got,
            answer: Answer::Kv(result),
        }) if got == id => result,
        Next::Frame(other) => protocol_error(&format!("expected answer {id}, got {other:?}")),
    }
}

/// A keyvalue request sent as CBOR.
fn kv_cold(ctx: &mut Ctx, request: KvRequest) -> Result<KvAnswer, KvError> {
    #[cfg(target_os = "linux")]
    if let Some(answer) = published_read(&request) {
        return Ok(answer);
    }
    #[cfg(target_os = "linux")]
    let wrote = written_bucket(&request);
    let result = match ctx.request(Request::Kv(request)) {
        Answer::Kv(result) => result,
        other => unexpected(&other),
    };
    #[cfg(target_os = "linux")]
    if let (Ok(_), Some(bucket)) = (&result, wrote) {
        note_write(ctx, bucket);
    }
    result
}

/// The answer to a read (`get`, `get_many`, `exists`) of a published
/// bucket, from its snapshot, unless this instance wrote past it.
#[cfg(target_os = "linux")]
fn published_read(request: &KvRequest) -> Option<KvAnswer> {
    match request {
        KvRequest::Get { bucket, key } => published_key_read(bucket, KvOp::Get, key),
        KvRequest::Exists { bucket, key } => published_key_read(bucket, KvOp::Exists, key),
        KvRequest::GetMany { bucket, keys } => with_snapshot(bucket, |snap| {
            KvAnswer::Values(
                keys.iter()
                    .map(|k| snap.get(k.as_bytes()).map(Bytes::from))
                    .collect(),
            )
        }),
        _ => None,
    }
}

/// A `get` or `exists` of a published bucket, from its snapshot, unless
/// this instance wrote past it.
#[cfg(target_os = "linux")]
fn published_key_read(bucket: &str, op: KvOp, key: &str) -> Option<KvAnswer> {
    match op {
        KvOp::Get => with_snapshot(bucket, |snap| {
            KvAnswer::Value(snap.get(key.as_bytes()).map(Bytes::from))
        }),
        KvOp::Exists => with_snapshot(bucket, |snap| {
            KvAnswer::Exists(snap.get(key.as_bytes()).is_some())
        }),
        KvOp::Set | KvOp::Delete => None,
    }
}

/// `f` on the snapshot `bucket` is read from, if it is published and this
/// instance did not write past it.
#[cfg(target_os = "linux")]
fn with_snapshot(
    bucket: &str,
    f: impl FnOnce(&crate::snapshot::Snapshot<'_>) -> KvAnswer,
) -> Option<KvAnswer> {
    with_conn(|conn| {
        let published = conn.published.get(bucket)?;
        if conn
            .written
            .get(bucket)
            .is_some_and(|w| published.revision < *w)
        {
            return None;
        }
        let snap = crate::snapshot::Snapshot::parse(published.mapped.bytes()).ok()?;
        Some(f(&snap))
    })
}

/// The published bucket a write goes to, if it goes to one.
#[cfg(target_os = "linux")]
fn written_bucket(request: &KvRequest) -> Option<String> {
    let bucket = match request {
        KvRequest::Set { bucket, .. }
        | KvRequest::Delete { bucket, .. }
        | KvRequest::Increment { bucket, .. }
        | KvRequest::SetMany { bucket, .. }
        | KvRequest::DeleteMany { bucket, .. }
        | KvRequest::CompareAndSwap { bucket, .. }
        | KvRequest::CompareAndDelete { bucket, .. } => bucket,
        _ => return None,
    };
    with_conn(|conn| conn.published.contains_key(bucket)).then(|| bucket.clone())
}

/// This instance wrote `bucket`: its reads go to the node until a
/// snapshot holds the bucket's revision now (which covers the write). A
/// failed ask leaves them there.
#[cfg(target_os = "linux")]
fn note_write(ctx: &mut Ctx, bucket: String) {
    let after = match ctx.request(Request::Kv(KvRequest::Revision {
        bucket: bucket.clone(),
    })) {
        Answer::Kv(Ok(KvAnswer::Revision(revision))) => revision,
        _ => u64::MAX,
    };
    with_conn(|conn| conn.written.insert(bucket, after));
}

/// A `get` on bucket handle `handle` as given, opened or not: for tests
/// of the node's checks on handles.
///
/// # Errors
///
/// The [`KvError`] the node answers.
#[doc(hidden)]
pub fn get_on_handle_for_tests(handle: u32, key: &str) -> Result<Option<Vec<u8>>, KvError> {
    #[cfg(target_os = "linux")]
    {
        const NAME: &str = "\0handle under test";
        with_conn(|conn| conn.handles.insert(NAME.to_string(), handle));
        let got = hot_request(&mut Ctx::current(), NAME, KvOp::Get, key, &[]);
        with_conn(|conn| conn.handles.remove(NAME));
        match got? {
            KvAnswer::Value(v) => Ok(v.map(|b| b.0)),
            other => unexpected_kv(&other),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (handle, key);
        Err(KvError::Other("linux only".into()))
    }
}

/// The revision of the snapshot this instance reads `bucket` from, if it
/// is published to it: for tests.
#[doc(hidden)]
#[must_use]
pub fn published_revision(bucket: &str) -> Option<u64> {
    #[cfg(target_os = "linux")]
    return with_conn(|conn| conn.published.get(bucket).map(|p| p.revision));
    #[cfg(not(target_os = "linux"))]
    {
        let _ = bucket;
        None
    }
}

fn unexpected_kv(answer: &KvAnswer) -> ! {
    protocol_error(&format!("unexpected keyvalue answer {answer:?}"))
}

/// A bucket, from [`Ctx::bucket`]: `bulkhead:keyvalue`'s operations and
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
        match kv_op(self.ctx, &self.name, KvOp::Get, key, &[])? {
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
        kv_op(self.ctx, &self.name, KvOp::Set, key, value).map(|_| ())
    }

    /// Delete a key; deleting a missing key succeeds.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn delete(&mut self, key: &str) -> Result<(), KvError> {
        kv_op(self.ctx, &self.name, KvOp::Delete, key, &[]).map(|_| ())
    }

    /// Whether a key exists.
    ///
    /// # Errors
    ///
    /// As for [`Bucket`].
    pub fn exists(&mut self, key: &str) -> Result<bool, KvError> {
        match kv_op(self.ctx, &self.name, KvOp::Exists, key, &[])? {
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

/// The node closed the channel: it stopped this instance.
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

/// The descriptor a frame from the node names, from any thread: each can
/// be taken once.
///
/// # Errors
///
/// The node passed no such descriptor, the app runs without a node, or
/// on a system without native apps.
pub fn take_fd(fd: FdRef) -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    if let Some(end) = END.get() {
        return end.take_fd(fd.0);
    }
    let _ = fd;
    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        "no node has passed descriptors to this process",
    ))
}

/// Break this process's ring to the node, as an app writing nonsense
/// there would: for tests of the node's checks only.
#[doc(hidden)]
pub fn break_channel_for_tests() {
    #[cfg(target_os = "linux")]
    if let Some(end) = END.get() {
        end.break_for_tests();
    }
}

/// Open the channel the node handed this process.
#[cfg(target_os = "linux")]
fn open_channel() -> Result<(), String> {
    let end = AppEnd::inherited().map_err(|e| e.to_string())?;
    END.set(end).map_err(|_| "served twice".to_string())
}

/// Native apps run on Linux only.
#[cfg(not(target_os = "linux"))]
fn open_channel() -> Result<(), String> {
    Err("native apps run on Linux only".into())
}

/// Serve `A` to the node over its channel until the node stops it.
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
    if let Err(e) = open_channel() {
        eprintln!("bulkhead-native: channel: {e}");
        return ExitCode::from(2);
    }
    send(&FromApp::Hello { version: VERSION });
    CONN.with(|cell| {
        *cell.borrow_mut() = Some(Conn {
            next_id: 0,
            metrics: Vec::new(),
            #[cfg(target_os = "linux")]
            published: HashMap::new(),
            #[cfg(target_os = "linux")]
            written: HashMap::new(),
            #[cfg(target_os = "linux")]
            handles: HashMap::new(),
            #[cfg(target_os = "linux")]
            inbuf: Vec::new(),
            #[cfg(target_os = "linux")]
            in_len: 0,
            #[cfg(target_os = "linux")]
            out: Vec::new(),
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
        between_callbacks(true);
        let frame = next_frame();
        between_callbacks(false);
        match frame {
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
            ToApp::Connection { listener, stream } => {
                guarded(|| app.handle_connection(&mut ctx, listener, stream));
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
