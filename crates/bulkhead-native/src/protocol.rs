//! The frames between a node and a native app, over the app's stdin and
//! stdout.
//!
//! Each frame is a 4-byte big-endian length, then that many bytes of CBOR:
//! a [`ToApp`] from the node, a [`FromApp`] from the app. The app speaks
//! first, with [`FromApp::Hello`] once it is ready; the node then sends
//! `Init` and, one at a time, the callbacks, each answered by one frame
//! (`Init`, `Reply` or `Done`, or `Failed`). While a callback runs the
//! app may send `Request`s, each answered by an `Answer` with its id, and
//! `Log`s at any time. The node sends `Ping` only while no callback runs,
//! answered by `Pong` with its id; an app answers one that reaches it
//! during a request as well.
//!
//! The types mirror `bulkhead:runtime`'s (pids, envelopes, exit reasons,
//! info, call errors). Version 2 added the keyvalue service
//! ([`Request::Kv`], [`Info::KvChanged`]).

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// The protocol version this crate speaks.
pub const VERSION: u16 = 2;

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

/// Why a name could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegisterError {
    /// The manifest does not list the name.
    AccessDenied,
    /// Another instance holds it.
    Taken(Pid),
}

/// A log record's level, as `wasi:logging`'s.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToApp {
    /// Start: answered by [`FromApp::Init`].
    Init {
        /// The app's init arguments (a supervisor child's `start_args` as
        /// JSON, or empty).
        #[serde(with = "bytes")]
        args: Vec<u8>,
    },
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
}

/// A frame from the app to the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FromApp {
    /// The first frame: the app is ready, and speaks `version`.
    Hello {
        /// The protocol version.
        version: u16,
    },
    /// `Init`'s outcome: an error ends the instance.
    Init(Result<(), String>),
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
}

/// What a native app asks of the node: `bulkhead:runtime/process`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// This instance's pid: [`Answer::Pid`].
    SelfPid,
    /// Send a message without waiting: [`Answer::Unit`].
    Cast {
        /// To whom.
        to: Pid,
        /// The message.
        msg: Envelope,
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
    /// A keyvalue operation (`wasi:keyvalue` and `bulkhead:watch`):
    /// [`Answer::Kv`].
    Kv(KvRequest),
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A registration's outcome.
    Registered(Result<(), RegisterError>),
    /// A keyvalue operation's outcome.
    Kv(Result<KvAnswer, KvError>),
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
