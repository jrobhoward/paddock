//! The frames between a node and a native app, over the app's stdin and
//! stdout.
//!
//! Each frame is a 4-byte big-endian length, then that many bytes of CBOR:
//! a [`ToApp`] from the node, a [`FromApp`] from the app. The app speaks
//! first, with [`FromApp::Hello`] once it is ready; the node then sends
//! `Init` (or `LoadSnapshot`, to start from a snapshot) and, one at a
//! time, the callbacks and `SaveSnapshot`, each answered by one frame
//! (`Init`, `Reply`, `Done` or `Snapshot`, or `Failed`). While a callback runs the
//! app may send `Request`s, each answered by an `Answer` with its id, and
//! `Log`s at any time. The node sends `Ping` only while no callback runs,
//! answered by `Pong` with its id; an app answers one that reaches it
//! during a request as well.
//!
//! The types mirror `bulkhead:runtime`'s (pids, envelopes, exit reasons,
//! info, call errors). Version 2 added the host services a native app may
//! declare: keyvalue ([`Request::Kv`], [`Info::KvChanged`]), groups,
//! config, secrets and metrics. Version 3 added snapshots
//! ([`ToApp::SaveSnapshot`], [`ToApp::LoadSnapshot`]), for snapshot
//! upgrades, passivation and moving a singleton with its state. Version 5
//! added bucket handles ([`Request::KvOpen`]) and the hot frames of
//! [`hot`](crate::hot): the most frequent keyvalue requests and their
//! answers in fixed layouts, beside the CBOR frames. Version 6 answers a
//! cast that was not queued with [`Answer::CastFailed`] instead of
//! [`Answer::Unit`], so an app can learn why (ROADMAP I1). Version 7 added
//! [`Request::CastWait`], a cast that waits for room in a full mailbox
//! (ROADMAP I2). Versions 8 to 10 added SQL, sockets and HTTP egress
//! requests (ROADMAP Z8d to Z8f), version 11 [`ToApp::Connection`],
//! a connection accepted on one of the app's listeners (ROADMAP Z8g), and
//! version 12 [`Request::Extension`], a call to a function of an
//! extension the node runs (ROADMAP Z8h), and hot layouts for
//! [`Request::CastWait`] and [`Answer::CastFailed`] (ROADMAP I7), which a
//! node writes only to an app at 12.
//!
//! A node accepts apps on [`VERSION`] and on [`MIN_VERSION`], the one
//! before, and sends an app only frames its version has
//! ([`ToApp::since`]); within that window frames change only by adding
//! (ROADMAP Z9a; the rules are DESIGN.md's "The protocol as the
//! contract", and `tests/protocol_versions.rs` holds both versions' frames
//! as golden bytes).

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// The protocol version this crate speaks.
pub const VERSION: u16 = 12;

/// The oldest version a node accepts from an app: the one before
/// [`VERSION`], so an app survives one node upgrade without a rebuild
/// (ROADMAP Z9a; the rules for a change are in DESIGN.md, "The protocol as
/// the contract").
pub const MIN_VERSION: u16 = VERSION - 1;

/// Whether a node speaking [`VERSION`] accepts an app that says it speaks
/// `version`.
#[must_use]
pub fn accepts(version: u16) -> bool {
    (MIN_VERSION..=VERSION).contains(&version)
}

/// The environment variable naming the directory of the bundle's `priv/`
/// files, set when the bundle has any: a native app reads them there.
pub const PRIV_ENV: &str = "BULKHEAD_PRIV";

/// The largest frame either side accepts: a 16 MiB message, with room for
/// the frame around it.
pub const MAX_FRAME: usize = 17 << 20;

/// An instance's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Pid {
    /// Node hosting the instance.
    pub node: u32,
    /// Instance id, unique on its node and stable across restarts.
    pub id: u64,
    /// Restart count, starting at 1.
    pub incarnation: u32,
}

/// A message: a content type, an optional schema id and the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// How to decode `body`.
    pub content_type: String,
    /// Schema id from the app's message catalog, if any.
    pub schema: Option<String>,
    /// The encoded message.
    #[serde(with = "bytes")]
    pub body: Vec<u8>,
}

impl Envelope {
    /// An envelope with no schema id.
    #[must_use]
    pub fn new(content_type: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            content_type: content_type.into(),
            schema: None,
            body: body.into(),
        }
    }
}

/// A descriptor the node passed on the app's descriptor socket, named by
/// the frame it goes with (`channel` module, ROADMAP T3). The node sends
/// the descriptor before the frame; the app takes it with
/// `app::take_fd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FdRef(pub u64);

/// Bytes, encoded as a CBOR byte string wherever they sit (in an
/// `Option`, a list or a pair).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Bytes(#[serde(with = "bytes")] pub Vec<u8>);

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl From<&[u8]> for Bytes {
    fn from(v: &[u8]) -> Self {
        Self(v.to_vec())
    }
}

/// Why an instance exited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    /// A clean stop.
    Normal,
    /// Stopped by its supervisor or the node.
    Shutdown,
    /// Killed; cannot be trapped.
    Killed,
    /// The instance did not exist.
    Noproc,
    /// The node hosting it was lost.
    Noconnection,
    /// A callback ran past its deadline.
    Timeout,
    /// It failed, and why.
    Crashed(String),
}

/// Something the node delivers to `handle_info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Info {
    /// A linked instance exited and this one traps exits, or another
    /// instance sent an exit signal.
    Exit {
        /// Who exited or sent the signal.
        from: Pid,
        /// Why.
        reason: ExitReason,
    },
    /// A monitored instance exited.
    Down {
        /// The reference `monitor` returned.
        reference: u64,
        /// The instance that exited.
        pid: Pid,
        /// Why.
        reason: ExitReason,
    },
    /// A timer from `send_after` fired.
    Timer {
        /// The reference `send_after` returned.
        reference: u64,
        /// The message it was given.
        msg: Envelope,
    },
    /// A watched key changed ([`KvRequest::Watch`]), with what it holds
    /// now. Changes to one key are conflated while their notice waits, so
    /// a slow instance misses intermediate values but always gets the
    /// latest.
    KvChanged {
        /// The watch's id.
        watch: u64,
        /// The bucket, as the app named it.
        bucket: String,
        /// The key.
        key: String,
        /// Its value and the revision it was written at, read when the
        /// notice was delivered; `None` if it was deleted.
        entry: Option<(Bytes, u64)>,
        /// The bucket's revision the notice reported; at least the
        /// change that raised it.
        revision: u64,
    },
    /// A named event from the node (`log-level`, and others an app may
    /// ignore).
    HostEvent {
        /// The event's name.
        name: String,
        /// Its payload.
        msg: Envelope,
    },
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallError {
    /// No such instance.
    Noproc,
    /// Its node is not reachable.
    Noconnection,
    /// No reply within the timeout.
    Timeout,
    /// Its mailbox is full.
    Overloaded,
    /// The message is over the target's limit.
    TooLarge,
    /// The target's messaging ACL refuses this app.
    AccessDenied,
    /// The target exited before replying.
    Exited(ExitReason),
}

/// Why a cast was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastError {
    /// No such instance.
    Noproc,
    /// Its node is not reachable.
    Noconnection,
    /// Its mailbox is full, or the link to its node is over its budget.
    Overloaded,
    /// The message is over the target's limit.
    TooLarge,
    /// The target's messaging ACL refuses this app.
    AccessDenied,
    /// No room in the target's mailbox within the timeout
    /// ([`Request::CastWait`]); the message was not queued.
    Timeout,
}

/// Why a name could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegisterError {
    /// The manifest does not list the name.
    AccessDenied,
    /// Another instance holds it.
    Taken(Pid),
}

/// A log record's level, as `bulkhead:logging`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Level {
    /// Variables and flow of control.
    Trace,
    /// Of interest when debugging.
    Debug,
    /// Of interest when monitoring.
    Info,
    /// Something unexpected the app recovered from.
    Warn,
    /// Something failed.
    Error,
    /// The app may not continue.
    Critical,
}

/// A frame from the node to the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToApp {
    /// Start: answered by [`FromApp::Init`].
    Init {
        /// The app's init arguments (a supervisor child's `start_args` as
        /// JSON, or empty).
        #[serde(with = "bytes")]
        args: Vec<u8>,
    },
    /// Start from a snapshot instead of `Init`, the app's `load_snapshot`:
    /// answered by [`FromApp::Init`], whose error refuses the snapshot.
    LoadSnapshot {
        /// The app version that wrote it.
        from_version: String,
        /// What its `save_snapshot` returned.
        #[serde(with = "bytes")]
        snapshot: Vec<u8>,
    },
    /// Write the state, between callbacks, without changing it: answered
    /// by [`FromApp::Snapshot`].
    SaveSnapshot,
    /// A call: answered by [`FromApp::Reply`].
    Call {
        /// The call's reference, for a later `reply`.
        call_ref: u64,
        /// The message.
        msg: Envelope,
    },
    /// A cast: answered by [`FromApp::Done`].
    Cast {
        /// The message.
        msg: Envelope,
    },
    /// Something for `handle_info`: answered by [`FromApp::Done`].
    Info {
        /// What happened.
        info: Info,
    },
    /// A connection accepted on one of the app's listeners, its stream
    /// passed on the descriptor socket: answered by [`FromApp::Done`]. The
    /// app holds it as a stream from [`NetRequest::Connect`], watching it
    /// with [`NetRequest::Notify`] and closing it with
    /// [`NetRequest::Close`]; it does not count against `max_sockets`.
    Connection {
        /// The listener's name in the manifest.
        listener: String,
        /// The stream.
        stream: NetStream,
    },
    /// Stop: answered by [`FromApp::Done`], after which the app exits.
    Terminate {
        /// Why.
        reason: ExitReason,
    },
    /// The answer to the app's [`FromApp::Request`] with this id.
    Answer {
        /// The request's id.
        id: u64,
        /// The answer.
        answer: Answer,
    },
    /// Answered by [`FromApp::Pong`] with the same id, at any time.
    Ping {
        /// Returned in the pong.
        id: u64,
    },
    /// A published bucket's new snapshot (`snapshot`, ROADMAP T4): a
    /// sealed memfd the app maps read-only and reads `bucket` from, in
    /// place of the one before. Sent before `Init` and between callbacks;
    /// not answered.
    Published {
        /// The shared bucket.
        bucket: String,
        /// Every write at or below it is in the snapshot.
        revision: u64,
        /// The memfd, on the descriptor socket.
        fd: FdRef,
    },
}

impl ToApp {
    /// The protocol version this frame first appeared in, so a node sends
    /// an app only frames its version has. Frames older than the versions
    /// a node accepts give 1; a new frame gives the version it comes in.
    #[must_use]
    pub fn since(&self) -> u16 {
        match self {
            Self::Init { .. }
            | Self::Call { .. }
            | Self::Cast { .. }
            | Self::Terminate { .. }
            | Self::Answer { .. }
            | Self::Ping { .. }
            | Self::LoadSnapshot { .. }
            | Self::SaveSnapshot
            | Self::Published { .. } => 1,
            Self::Info { info } => info.since(),
            Self::Connection { .. } => 11,
        }
    }
}

impl Info {
    /// As [`ToApp::since`], for the info a `ToApp::Info` carries.
    #[must_use]
    pub fn since(&self) -> u16 {
        match self {
            Self::Exit { .. }
            | Self::Down { .. }
            | Self::Timer { .. }
            | Self::KvChanged { .. }
            | Self::HostEvent { .. } => 1,
        }
    }
}

/// A frame from the app to the node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromApp {
    /// The first frame: the app is ready, and speaks `version`.
    Hello {
        /// The protocol version.
        version: u16,
    },
    /// `Init`'s or `LoadSnapshot`'s outcome: an error ends the instance,
    /// or refuses the snapshot.
    Init(Result<(), String>),
    /// `SaveSnapshot`'s state.
    Snapshot(Bytes),
    /// A call's outcome: the reply, or `None` to reply later with
    /// [`Request::Reply`].
    Reply(Option<Envelope>),
    /// A cast, an info or terminate is done.
    Done,
    /// The callback failed (a panic): the instance crashes with this
    /// message.
    Failed(String),
    /// A request to the node during a callback, answered by
    /// [`ToApp::Answer`] with the same id.
    Request {
        /// Unique among the app's requests.
        id: u64,
        /// What it asks.
        request: Request,
    },
    /// A log record, at any time.
    Log {
        /// The record's level.
        level: Level,
        /// The message.
        message: String,
    },
    /// The answer to [`ToApp::Ping`].
    Pong {
        /// The ping's id.
        id: u64,
    },
    /// Metric updates, during a callback, answered by nothing: the app
    /// sends a callback's updates together, before its result, and the
    /// node logs any it refuses.
    Metrics(Vec<MetricUpdate>),
}

/// What a native app asks of the node: `bulkhead:runtime/process` and
/// the host services it may declare.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// This instance's pid: [`Answer::Pid`].
    SelfPid,
    /// Send a message without waiting: [`Answer::Unit`] once it is
    /// queued (in the target's mailbox, or for another node on the link to
    /// it), [`Answer::CastFailed`] if it was dropped.
    Cast {
        /// To whom.
        to: Pid,
        /// The message.
        msg: Envelope,
    },
    /// Send a message, waiting up to `timeout_ms` for room if the target's
    /// mailbox is full: [`Answer::Unit`] once it is queued,
    /// [`Answer::CastFailed`] if it was not (with [`CastError::Timeout`]
    /// when no room came). The node caps the wait at the instance's
    /// callback deadline.
    CastWait {
        /// To whom.
        to: Pid,
        /// The message.
        msg: Envelope,
        /// How long to wait for room.
        timeout_ms: u32,
    },
    /// Call and wait for the reply: [`Answer::Called`].
    Call {
        /// To whom.
        to: Pid,
        /// The message.
        msg: Envelope,
        /// How long to wait.
        timeout_ms: u32,
    },
    /// Answer a call taken with no reply: [`Answer::Unit`].
    Reply {
        /// The call's reference.
        call_ref: u64,
        /// The reply.
        msg: Envelope,
    },
    /// Link to another instance: [`Answer::Unit`].
    Link(Pid),
    /// Remove a link: [`Answer::Unit`].
    Unlink(Pid),
    /// Turn exit signals into [`Info::Exit`]: [`Answer::Unit`].
    TrapExits(bool),
    /// Monitor another instance: [`Answer::Ref`].
    Monitor(Pid),
    /// Remove a monitor: [`Answer::Unit`].
    Demonitor(u64),
    /// Register a name the manifest lists: [`Answer::Registered`].
    Register(String),
    /// Look up a registered name: [`Answer::MaybePid`].
    Whereis(String),
    /// Deliver `msg` as [`Info::Timer`] after `ms`: [`Answer::Ref`].
    SendAfter {
        /// The delay.
        ms: u32,
        /// The message.
        msg: Envelope,
    },
    /// Cancel a timer: [`Answer::Unit`].
    CancelTimer(u64),
    /// Send an exit signal: [`Answer::Unit`].
    Exit {
        /// To whom (this instance's own pid stops it).
        to: Pid,
        /// The reason.
        reason: ExitReason,
    },
    /// A keyvalue operation (`bulkhead:keyvalue` and `bulkhead:watch`):
    /// [`Answer::Kv`].
    Kv(KvRequest),
    /// A handle for a bucket the manifest lists, for the hot frames'
    /// keyvalue requests: [`Answer::KvOpened`]. Checked as a request
    /// naming the bucket is; the handle stays valid for the instance's
    /// life, and a request on a bucket the node no longer configures
    /// fails as one naming it would.
    KvOpen(String),
    /// A process groups operation (`bulkhead:groups`): [`Answer::Groups`].
    Groups(GroupsRequest),
    /// A configuration value (`bulkhead:config`): [`Answer::Config`].
    ConfigGet(String),
    /// Every configuration value: [`Answer::ConfigAll`].
    ConfigAll,
    /// A secret the manifest lists (`bulkhead:secrets`): [`Answer::Secret`].
    Secret(String),
    /// Register a metric (`bulkhead:metrics`), or find one registered with
    /// the same name and labels: [`Answer::Metric`], its id for
    /// [`MetricUpdate`]s.
    MetricRegister {
        /// Counter, gauge or histogram.
        kind: MetricKind,
        /// The name, after the app's prefix.
        name: String,
        /// The `# HELP` text.
        help: String,
        /// Label names and values.
        labels: Vec<(String, String)>,
        /// A histogram's bucket bounds; empty for the default ones.
        buckets: Vec<f64>,
    },
    /// A SQL operation (`bulkhead:sql`): [`Answer::Sql`].
    Sql(SqlRequest),
    /// A socket operation (`bulkhead:sockets`): [`Answer::Net`].
    Net(NetRequest),
    /// An HTTP request (`bulkhead:http-egress`), sent by the node's
    /// egress client on the manifest's rules: [`Answer::Http`].
    Http(HttpRequest),
    /// A call to a function of an extension the node runs (ROADMAP Z8h):
    /// [`Answer::Extension`]. The extension checks the app's grant and
    /// scope, as for a component's import of its package.
    Extension {
        /// The extension's package (`example:greeting`).
        package: String,
        /// The function, by the extension's name for it: by convention the
        /// WIT interface and function, `greet#hello`.
        function: String,
        /// The arguments in CBOR: by convention an array of the WIT
        /// parameters, in order.
        args: Bytes,
    },
}

/// Why an extension call was not made: the extension's own errors (a
/// denied grant, say) are in the result it returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtensionError {
    /// The node runs no extension serving this package.
    NoSuchExtension,
    /// The extension has no such function for native apps.
    NoSuchFunction(String),
    /// The arguments are not what the function takes.
    BadArguments(String),
}

/// An HTTP request a native app asks the node to send (ROADMAP Z8f), on a
/// component's terms: the `allow` list, the methods, the forbidden
/// headers, the app's requests in flight, no redirects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    /// The method (`GET`).
    pub method: String,
    /// The URL (`https://api.example.com/v1/x?y=1`).
    pub url: String,
    /// Header names and values.
    pub headers: Vec<(String, Bytes)>,
    /// The body.
    pub body: Bytes,
    /// The longest wait for the first byte and between bytes, shortened
    /// to node config's; `None` for node config's.
    pub timeout_ms: Option<u32>,
}

/// An HTTP response, whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// Header names and values.
    pub headers: Vec<(String, Bytes)>,
    /// The body.
    pub body: Bytes,
}

/// Why an HTTP request failed: as `bulkhead_sdk::http_client::HttpError`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpError {
    /// Not granted, or outside the manifest's `allow` list or methods.
    Denied,
    /// Over the app's requests in flight.
    Limited,
    /// The name resolves only to loopback or link-local addresses.
    Prohibited,
    /// No response, or a body chunk, in time.
    Timeout,
    /// The server could not be reached.
    Unreachable(String),
    /// TLS failed.
    Tls(String),
    /// The URL, method or a header is not valid, or a header is forbidden.
    Invalid(String),
    /// Anything else.
    Other(String),
}

/// A process groups operation, on groups the manifest lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupsRequest {
    /// Join a group: [`GroupsAnswer::Unit`].
    Join(String),
    /// Leave a group: [`GroupsAnswer::Unit`].
    Leave(String),
    /// A group's members across the cluster: [`GroupsAnswer::Members`].
    Members(String),
    /// Cast `msg` to every member: [`GroupsAnswer::Delivered`].
    Publish {
        /// The group.
        group: String,
        /// The message.
        msg: Envelope,
    },
}

/// A successful groups operation's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupsAnswer {
    /// Done.
    Unit,
    /// The group's members.
    Members(Vec<Pid>),
    /// How many members the message was queued for.
    Delivered(u32),
}

/// Why a groups operation failed: as `bulkhead:groups`'s `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupsError {
    /// Not granted, or a group the manifest does not list for this.
    AccessDenied,
    /// Over the instance's `limits.groups_max`.
    LimitReached,
}

/// Why a configuration read failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigError {
    /// `bulkhead:config` is not granted.
    AccessDenied,
    /// The configuration could not be read.
    Other(String),
}

/// Why a secret could not be read: as `bulkhead:secrets`'s `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretError {
    /// Not granted, or a name the manifest does not list.
    AccessDenied,
    /// The node binds no secret of that name.
    NotFound,
    /// The node could not read it.
    Unavailable(String),
}

/// A socket operation (`bulkhead:sockets`, ROADMAP Z8e), on the manifest's
/// rules. A TCP stream is made by the node and passed to the app on the
/// descriptor socket, connected; the node keeps a copy and shuts the
/// socket down when the app closes it. Listeners and UDP sockets stay in
/// the node, which answers for them. Sockets are named by the number the
/// node gives them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetRequest {
    /// Connect a TCP stream: [`NetAnswer::Stream`].
    Connect(std::net::SocketAddr),
    /// Connect to the first address `host` resolves to that accepts:
    /// [`NetAnswer::Stream`]. Only with `connect` rules.
    ConnectHost {
        /// The host name.
        host: String,
        /// The port.
        port: u16,
    },
    /// Bind and listen: [`NetAnswer::Listener`].
    Listen(std::net::SocketAddr),
    /// A connection waiting on a listener, without waiting:
    /// [`NetAnswer::Accepted`].
    Accept(u32),
    /// Bind a UDP socket: [`NetAnswer::Udp`].
    UdpBind(std::net::SocketAddr),
    /// Send a datagram from a UDP socket: [`NetAnswer::Unit`].
    UdpSend {
        /// The socket.
        socket: u32,
        /// Where to.
        to: std::net::SocketAddr,
        /// The datagram.
        data: Bytes,
    },
    /// A datagram already received, without waiting:
    /// [`NetAnswer::Datagram`].
    UdpRecv(u32),
    /// Deliver the host event `io-ready` with `tag` once the socket is
    /// readable (a stream's bytes or its close, a listener's connection, a
    /// datagram), once: [`NetAnswer::Unit`]. Replaces the socket's notice
    /// waiting, if any.
    Notify {
        /// The socket.
        socket: u32,
        /// The event's tag.
        tag: u64,
    },
    /// Close a socket, giving back its share of `max_sockets`:
    /// [`NetAnswer::Unit`].
    Close(u32),
}

/// A TCP stream the node made: its descriptor, passed on the descriptor
/// socket, and its addresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetStream {
    /// The socket's number.
    pub socket: u32,
    /// The descriptor.
    pub fd: FdRef,
    /// The local address.
    pub local: std::net::SocketAddr,
    /// The peer's address.
    pub peer: std::net::SocketAddr,
}

/// A successful socket operation's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetAnswer {
    /// Done.
    Unit,
    /// A connected stream.
    Stream(NetStream),
    /// A listener, and its bound address.
    Listener {
        /// The socket's number.
        socket: u32,
        /// The bound address.
        local: std::net::SocketAddr,
    },
    /// A connection accepted, or none waiting.
    Accepted(Option<NetStream>),
    /// A UDP socket, and its bound address.
    Udp {
        /// The socket's number.
        socket: u32,
        /// The bound address.
        local: std::net::SocketAddr,
    },
    /// A datagram and its sender, or none waiting.
    Datagram(Option<(std::net::SocketAddr, Bytes)>),
}

/// Why a socket operation failed: as `bulkhead_sdk::net::NetError`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetError {
    /// Not granted, or outside the manifest's rules.
    Denied,
    /// Over the app's `max_sockets`.
    Limited,
    /// The peer closed, or the socket was closed.
    Closed,
    /// Anything else.
    Other(String),
}

/// A SQL operation on a datasource the manifest lists (`bulkhead:sql`).
/// Parameters are bound (`$1`, `$2`), never interpolated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SqlRequest {
    /// A statement that returns rows: [`SqlAnswer::Rows`].
    Query {
        /// The datasource's name.
        datasource: String,
        /// The statement.
        statement: String,
        /// Its parameters.
        params: Vec<SqlValue>,
    },
    /// A statement run for its effect: [`SqlAnswer::Changed`].
    Execute {
        /// The datasource's name.
        datasource: String,
        /// The statement.
        statement: String,
        /// Its parameters.
        params: Vec<SqlValue>,
    },
    /// Begin a transaction on a datasource, holding one of the app's
    /// connections until it ends: [`SqlAnswer::Begun`].
    Begin(String),
    /// [`SqlRequest::Query`] inside a transaction.
    TxQuery {
        /// The transaction, as [`SqlAnswer::Begun`] named it.
        tx: u32,
        /// The statement.
        statement: String,
        /// Its parameters.
        params: Vec<SqlValue>,
    },
    /// [`SqlRequest::Execute`] inside a transaction.
    TxExecute {
        /// The transaction.
        tx: u32,
        /// The statement.
        statement: String,
        /// Its parameters.
        params: Vec<SqlValue>,
    },
    /// End a transaction, committing it or rolling it back:
    /// [`SqlAnswer::Ended`]. It has ended either way.
    TxEnd {
        /// The transaction.
        tx: u32,
        /// Commit, or roll back.
        commit: bool,
    },
}

/// A SQL value, as `bulkhead:sql`'s `value`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SqlValue {
    /// `NULL`.
    Null,
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// Text (dates, `numeric`, `json` and `uuid` arrive as text).
    Text(String),
    /// `bytea`.
    Bytes(Bytes),
}

/// A query's result.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SqlRows {
    /// The columns' names.
    pub columns: Vec<String>,
    /// The rows, each a value per column.
    pub rows: Vec<Vec<SqlValue>>,
}

/// A successful SQL operation's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SqlAnswer {
    /// A query's rows.
    Rows(SqlRows),
    /// How many rows a statement changed.
    Changed(u64),
    /// A transaction begun: its handle for this instance.
    Begun(u32),
    /// A transaction ended.
    Ended,
}

/// Why a SQL operation failed: as `bulkhead:sql`'s `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SqlError {
    /// Not granted, or a datasource the manifest does not list.
    AccessDenied,
    /// The node binds no datasource of that name.
    NotFound,
    /// No connection free in time: the app's cap or the node's pool.
    LimitReached,
    /// The database could not be reached, or the transaction has ended.
    Unavailable(String),
    /// The statement ran past the datasource's statement timeout.
    Timeout,
    /// The result is larger than the node allows.
    TooLarge,
    /// A column of a type the interface does not carry.
    Unsupported(String),
    /// The database refused the statement.
    Query {
        /// Its SQLSTATE code.
        code: String,
        /// The server's message.
        message: String,
    },
}

/// A metric's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricKind {
    /// Only goes up.
    Counter,
    /// Goes up and down.
    Gauge,
    /// Counts observations into buckets.
    Histogram,
}

/// One metric update, by the id [`Request::MetricRegister`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MetricUpdate {
    /// Add to a counter or gauge.
    Add(u32, f64),
    /// Set a gauge.
    Set(u32, f64),
    /// Record a histogram observation.
    Observe(u32, f64),
}

/// Why a metric operation failed: as `bulkhead:metrics`'s `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricError {
    /// `bulkhead:metrics` is not granted.
    AccessDenied,
    /// A bad name, label, bucket or value, and why.
    Invalid(String),
    /// Over the app's series limit.
    LimitReached,
    /// No metric with that id.
    UnknownMetric,
}

/// A keyvalue operation on a bucket the manifest lists, named on each
/// request. Its answer, on success, is the [`KvAnswer`] each names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KvRequest {
    /// A key's value: [`KvAnswer::Value`].
    Get {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
    },
    /// Set a key: [`KvAnswer::Unit`].
    Set {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// Its new value.
        value: Bytes,
    },
    /// Delete a key: [`KvAnswer::Unit`].
    Delete {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
    },
    /// Whether a key exists: [`KvAnswer::Exists`].
    Exists {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
    },
    /// A page of the bucket's keys: [`KvAnswer::Keys`].
    Keys {
        /// The bucket.
        bucket: String,
        /// The previous page's cursor; `None` for the first page.
        cursor: Option<u64>,
    },
    /// Add `delta` to a counter: [`KvAnswer::Count`].
    Increment {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// How much to add.
        delta: u64,
    },
    /// Several keys' values in one request: [`KvAnswer::Values`].
    GetMany {
        /// The bucket.
        bucket: String,
        /// The keys.
        keys: Vec<String>,
    },
    /// Set several keys as one change: [`KvAnswer::Unit`].
    SetMany {
        /// The bucket.
        bucket: String,
        /// Keys and their values.
        entries: Vec<(String, Bytes)>,
    },
    /// Delete several keys as one change: [`KvAnswer::Unit`].
    DeleteMany {
        /// The bucket.
        bucket: String,
        /// The keys.
        keys: Vec<String>,
    },
    /// A key's value and revision: [`KvAnswer::Entry`].
    Entry {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// Served by a replicated bucket's primary, reflecting every write
        /// committed before the request.
        linearizable: bool,
    },
    /// The bucket's revision: [`KvAnswer::Revision`].
    Revision {
        /// The bucket.
        bucket: String,
    },
    /// One page of the bucket's entries in key order: [`KvAnswer::Page`].
    Scan {
        /// The bucket.
        bucket: String,
        /// Which entries.
        query: ScanQuery,
    },
    /// Set a key if its revision is `expected` (`None`: only if absent):
    /// [`KvAnswer::Revision`], the new one.
    CompareAndSwap {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// The revision it must have.
        expected: Option<u64>,
        /// Its new value.
        value: Bytes,
    },
    /// Delete a key if its revision is `expected`: [`KvAnswer::Revision`],
    /// the bucket's new one.
    CompareAndDelete {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// The revision it must have.
        expected: u64,
    },
    /// Watch a key, or every key starting with it: [`KvAnswer::Started`].
    /// Changes arrive as [`Info::KvChanged`].
    Watch {
        /// The bucket.
        bucket: String,
        /// The key or prefix.
        key: String,
        /// Every key starting with `key`.
        prefix: bool,
        /// First notice every matching key changed after this revision,
        /// or fail with [`KvError::TooOld`].
        after: Option<u64>,
    },
    /// End a watch: [`KvAnswer::Unit`].
    Unwatch(u64),
}

/// What a scan reads: as `bulkhead:watch`'s `scan-query`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScanQuery {
    /// Only keys starting with this.
    pub prefix: Option<String>,
    /// Only keys from this one on (inclusive).
    pub start: Option<String>,
    /// Only keys before this one (exclusive).
    pub end: Option<String>,
    /// Downward, from the last key in range.
    pub reverse: bool,
    /// At most this many entries; 0 is the node's page size (1,000).
    pub limit: u32,
    /// The previous page's `next`.
    pub cursor: Option<String>,
}

/// One page of a scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanPage {
    /// Key, value and revision, in key order (downward with `reverse`).
    pub entries: Vec<(String, Bytes, u64)>,
    /// Where the next page starts; `None` when the scan is complete.
    pub next: Option<String>,
    /// The bucket's revision when the page was read: watching from it
    /// sees every change the page could not.
    pub revision: u64,
}

/// A successful keyvalue operation's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KvAnswer {
    /// Done; nothing to return.
    Unit,
    /// A value, if the key exists.
    Value(Option<Bytes>),
    /// Whether the key exists.
    Exists(bool),
    /// A page of keys, and the cursor of the next page.
    Keys(Vec<String>, Option<u64>),
    /// A counter's new value.
    Count(u64),
    /// Each key's value, in the order asked; `None` for a missing key.
    Values(Vec<Option<Bytes>>),
    /// A value and its revision, if the key exists.
    Entry(Option<(Bytes, u64)>),
    /// A revision.
    Revision(u64),
    /// A page of a scan.
    Page(ScanPage),
    /// A started watch: its id and the bucket's revision then.
    Started {
        /// For [`KvRequest::Unwatch`] and in its notices.
        id: u64,
        /// The bucket's revision when the watch started.
        revision: u64,
    },
}

/// Why a keyvalue operation failed: as `bulkhead:watch`'s `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KvError {
    /// Not granted, not in the manifest, or a write to a read-only shared
    /// bucket.
    AccessDenied,
    /// A shared bucket the node does not configure.
    NoSuchBucket,
    /// Over the instance's `limits.watches_max`.
    LimitReached,
    /// The bucket cannot replay changes after that revision; carries its
    /// current one.
    TooOld(u64),
    /// Compare-and-swap found another revision (`None`: the key is absent).
    Conflict(Option<u64>),
    /// A replicated bucket with no primary that can take the operation.
    Unavailable,
    /// Storage failed, or a quota was exceeded.
    Other(String),
}

/// The node's answer to a [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Answer {
    /// Done; nothing to return.
    Unit,
    /// A pid.
    Pid(Pid),
    /// A pid, if the name is registered.
    MaybePid(Option<Pid>),
    /// A monitor or timer reference.
    Ref(u64),
    /// A call's reply or error.
    Called(Result<Envelope, CallError>),
    /// Why a cast was dropped.
    CastFailed(CastError),
    /// A registration's outcome.
    Registered(Result<(), RegisterError>),
    /// A keyvalue operation's outcome.
    Kv(Result<KvAnswer, KvError>),
    /// A bucket's handle, or why it does not open.
    KvOpened(Result<u32, KvError>),
    /// A groups operation's outcome.
    Groups(Result<GroupsAnswer, GroupsError>),
    /// A configuration value, if set.
    Config(Result<Option<String>, ConfigError>),
    /// Every configuration value.
    ConfigAll(Result<Vec<(String, String)>, ConfigError>),
    /// A secret's bytes.
    Secret(Result<Bytes, SecretError>),
    /// A registered metric's id.
    Metric(Result<u32, MetricError>),
    /// A SQL operation's result.
    Sql(Result<SqlAnswer, SqlError>),
    /// A socket operation's result.
    Net(Result<NetAnswer, NetError>),
    /// An HTTP request's response.
    Http(Result<HttpResponse, HttpError>),
    /// An extension function's result, in CBOR.
    Extension(Result<Bytes, ExtensionError>),
}

/// A frame that could not be read.
#[derive(Debug)]
pub enum FrameError {
    /// The stream failed.
    Io(io::Error),
    /// The frame is longer than [`MAX_FRAME`].
    TooLarge(usize),
    /// The frame is not valid CBOR for the expected type.
    Decode(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::TooLarge(n) => write!(f, "frame of {n} bytes is over {MAX_FRAME}"),
            Self::Decode(e) => write!(f, "malformed frame: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Encode `frame` with its length prefix.
#[must_use]
pub fn encode<T: Serialize>(frame: &T) -> Vec<u8> {
    let mut out = vec![0; 4];
    // Writing to a Vec does not fail, and these types always serialize.
    let _ = ciborium::into_writer(frame, &mut out);
    let len = u32::try_from(out.len() - 4).unwrap_or(u32::MAX);
    out[..4].copy_from_slice(&len.to_be_bytes());
    out
}

/// Decode a frame's body (without its length prefix).
///
/// # Errors
///
/// [`FrameError::Decode`] for bytes that are not a `T`.
pub fn decode<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, FrameError> {
    ciborium::from_reader(body).map_err(|e| FrameError::Decode(e.to_string()))
}

/// The length a frame's 4-byte prefix gives.
///
/// # Errors
///
/// [`FrameError::TooLarge`] over [`MAX_FRAME`].
pub fn frame_len(prefix: [u8; 4]) -> Result<usize, FrameError> {
    let len = usize::try_from(u32::from_be_bytes(prefix)).unwrap_or(usize::MAX);
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    Ok(len)
}

/// Read one frame; `None` at the end of the stream.
///
/// # Errors
///
/// A [`FrameError`] for a failed stream or a frame that does not decode.
pub fn read<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> Result<Option<T>, FrameError> {
    let mut prefix = [0u8; 4];
    match r.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(FrameError::Io(e)),
    }
    let mut body = vec![0; frame_len(prefix)?];
    r.read_exact(&mut body).map_err(FrameError::Io)?;
    decode(&body).map(Some)
}

/// Write one frame and flush.
///
/// # Errors
///
/// The stream's error.
pub fn write<T: Serialize>(w: &mut impl Write, frame: &T) -> io::Result<()> {
    w.write_all(&encode(frame))?;
    w.flush()
}

/// Byte strings as CBOR byte strings, not arrays.
mod bytes {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    struct Bytes;

    impl<'de> Visitor<'de> for Bytes {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bytes")
        }

        fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
            Ok(v.to_vec())
        }

        fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
            Ok(v)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            let mut out = Vec::new();
            while let Some(b) = seq.next_element::<u8>()? {
                out.push(b);
            }
            Ok(out)
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        d.deserialize_byte_buf(Bytes)
    }
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod protocol_tests;
