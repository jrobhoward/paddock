//! Hot frames: the frames sent most often, in fixed layouts instead of
//! CBOR (ROADMAP T7).
//!
//! A frame is the same 4-byte big-endian length as any other; its body
//! starts with a tag byte. A tag from `0x01` to `0x17` is a hot frame,
//! laid out below. Any other first byte is CBOR, as in [`protocol`]: a
//! frame type there is a serde enum, whose encoding starts with a text
//! string (a unit variant, `0x60` to `0x7f`) or a map of one entry
//! (`0xa1`), never with a small unsigned integer (`0x00` to `0x17`).
//!
//! Integers are little endian. From the app:
//!
//! | Tag | Frame | Fields after the tag |
//! | --- | --- | --- |
//! | `0x01` | keyvalue get | id `u64`, bucket `u32`, key length `u16`, key |
//! | `0x02` | keyvalue set | id `u64`, bucket `u32`, key length `u16`, key, value length `u32`, value |
//! | `0x03` | keyvalue delete | id `u64`, bucket `u32`, key length `u16`, key |
//! | `0x04` | keyvalue exists | id `u64`, bucket `u32`, key length `u16`, key |
//! | `0x05` | [`FromApp::Done`] | none |
//! | `0x06` | [`FromApp::Reply`] with no reply | none |
//! | `0x07` | [`FromApp::Reply`] | envelope |
//! | `0x08` | [`Request::Cast`] | id `u64`, pid, envelope |
//! | `0x09` | [`Request::Call`] | id `u64`, pid, timeout in ms `u32`, envelope |
//! | `0x0a` | [`Request::Reply`] | id `u64`, call reference `u64`, envelope |
//! | `0x0b` | [`Request::CastWait`] | id `u64`, pid, timeout in ms `u32`, envelope |
//!
//! The bucket is the handle [`Request::KvOpen`] returned; the key is UTF-8.
//! A keyvalue request is answered, with the request's id, by one of tags
//! `0x01` to `0x04` from the node, or by a CBOR [`ToApp::Answer`]
//! carrying the error. From the node:
//!
//! | Tag | Frame | Fields after the tag |
//! | --- | --- | --- |
//! | `0x01` | a keyvalue value | id `u64`, value length `u32`, value |
//! | `0x02` | no keyvalue value (the key is absent) | id `u64` |
//! | `0x03` | a keyvalue set or delete done | id `u64` |
//! | `0x04` | whether a key exists | id `u64`, `0` or `1` |
//! | `0x05` | [`ToApp::Call`] | call reference `u64`, envelope |
//! | `0x06` | [`ToApp::Cast`] | envelope |
//! | `0x07` | [`ToApp::Answer`] with [`Answer::Unit`] | id `u64` |
//! | `0x08` | [`ToApp::Answer`] with a call's reply | id `u64`, envelope |
//! | `0x09` | [`ToApp::Answer`] with [`Answer::CastFailed`] | id `u64`, reason `u8` |
//!
//! A cast's reason is `0` noproc, `1` noconnection, `2` overloaded, `3`
//! too large, `4` access denied, `5` timeout ([`CastError`]). Tags `0x0b`
//! from the app and `0x09` from the node came in protocol version 12
//! (ROADMAP I7), after the others: the node writes `0x09` only to an app
//! at 12 or later ([`write_to_app_at`]), and CBOR to an older one.
//!
//! A pid is its node `u32`, id `u64` and incarnation `u32`. An envelope is
//! its content type (length `u16`, then UTF-8), its schema id (length
//! `u16`, `0xffff` for none, then UTF-8) and its body (length `u32`, then
//! the bytes). Any other frame, and any of these whose strings do not fit
//! their lengths, is CBOR.
//!
//! [`protocol`]: crate::protocol
//! [`Request::KvOpen`]: crate::protocol::Request::KvOpen
//! [`Request::Cast`]: crate::protocol::Request::Cast
//! [`Request::Call`]: crate::protocol::Request::Call
//! [`Request::Reply`]: crate::protocol::Request::Reply
//! [`FromApp::Done`]: crate::protocol::FromApp::Done
//! [`FromApp::Reply`]: crate::protocol::FromApp::Reply
//! [`ToApp::Answer`]: crate::protocol::ToApp::Answer
//! [`ToApp::Call`]: crate::protocol::ToApp::Call
//! [`ToApp::Cast`]: crate::protocol::ToApp::Cast
//! [`Answer::Unit`]: crate::protocol::Answer::Unit
//! [`Answer::CastFailed`]: crate::protocol::Answer::CastFailed
//! [`Request::CastWait`]: crate::protocol::Request::CastWait
//! [`CastError`]: crate::protocol::CastError

use crate::protocol::{
    Answer, CastError, Envelope, FrameError, FromApp, MAX_FRAME, Pid, Request, ToApp, VERSION,
};

/// The highest tag byte of a hot frame; `0x01` is the lowest.
pub const LAST_TAG: u8 = 0x17;

const KV_GET: u8 = 0x01;
const KV_SET: u8 = 0x02;
const KV_DELETE: u8 = 0x03;
const KV_EXISTS: u8 = 0x04;

const ANSWER_VALUE: u8 = 0x01;
const ANSWER_NONE: u8 = 0x02;
const ANSWER_UNIT: u8 = 0x03;
const ANSWER_EXISTS: u8 = 0x04;

const DONE: u8 = 0x05;
const REPLY_NONE: u8 = 0x06;
const REPLY: u8 = 0x07;
const CAST_TO: u8 = 0x08;
const CALL_TO: u8 = 0x09;
const REPLY_TO: u8 = 0x0a;
const CAST_WAIT_TO: u8 = 0x0b;

const CALL: u8 = 0x05;
const CAST: u8 = 0x06;
const ANSWER_DONE: u8 = 0x07;
const ANSWER_CALLED: u8 = 0x08;
const ANSWER_CAST_FAILED: u8 = 0x09;

/// The protocol version that added the waited cast's and the failed cast's
/// hot layouts.
const CAST_LAYOUTS_SINCE: u16 = 12;

/// An envelope's absent schema id.
const NO_SCHEMA: u16 = u16::MAX;

/// Whether a frame's body is a hot frame rather than CBOR.
#[must_use]
pub fn is_hot(body: &[u8]) -> bool {
    body.first().is_some_and(|tag| (1..=LAST_TAG).contains(tag))
}

/// A keyvalue operation a hot frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvOp {
    /// A key's value.
    Get,
    /// Set a key to the frame's value.
    Set,
    /// Delete a key.
    Delete,
    /// Whether a key exists.
    Exists,
}

/// A keyvalue request, borrowed from its frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvFrame<'a> {
    /// The request's id.
    pub id: u64,
    /// The bucket's handle.
    pub bucket: u32,
    /// The operation.
    pub op: KvOp,
    /// The key.
    pub key: &'a str,
    /// The value to set; empty for the other operations.
    pub value: &'a [u8],
}

/// A keyvalue answer, borrowed from its frame (or from the bucket, when
/// the node writes one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvReply<'a> {
    /// A get's value, if the key exists.
    Value(Option<&'a [u8]>),
    /// A set or delete is done.
    Unit,
    /// Whether the key exists.
    Exists(bool),
}

/// Append a frame to `out`: its length prefix, then what `body` writes.
fn frame_into(out: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    body(out);
    let len = u32::try_from(out.len() - start - 4).unwrap_or(u32::MAX);
    out[start..start + 4].copy_from_slice(&len.to_be_bytes());
}

/// Append a keyvalue request frame to `out`. `None`, writing nothing, for
/// a key or value too long for the layout: send it as CBOR instead.
#[must_use]
pub fn write_kv_request(out: &mut Vec<u8>, request: &KvFrame<'_>) -> Option<()> {
    let key_len = u16::try_from(request.key.len()).ok()?;
    let value_len = u32::try_from(request.value.len()).ok()?;
    if request.key.len() + request.value.len() + 23 > MAX_FRAME {
        return None;
    }
    frame_into(out, |out| {
        out.push(match request.op {
            KvOp::Get => KV_GET,
            KvOp::Set => KV_SET,
            KvOp::Delete => KV_DELETE,
            KvOp::Exists => KV_EXISTS,
        });
        out.extend_from_slice(&request.id.to_le_bytes());
        out.extend_from_slice(&request.bucket.to_le_bytes());
        out.extend_from_slice(&key_len.to_le_bytes());
        out.extend_from_slice(request.key.as_bytes());
        if request.op == KvOp::Set {
            out.extend_from_slice(&value_len.to_le_bytes());
            out.extend_from_slice(request.value);
        }
    });
    Some(())
}

/// Append a keyvalue answer frame to `out`, for the request `id`.
pub fn write_kv_answer(out: &mut Vec<u8>, id: u64, reply: &KvReply<'_>) {
    frame_into(out, |out| match *reply {
        KvReply::Value(Some(value)) => {
            out.push(ANSWER_VALUE);
            out.extend_from_slice(&id.to_le_bytes());
            let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(value);
        }
        KvReply::Value(None) => {
            out.push(ANSWER_NONE);
            out.extend_from_slice(&id.to_le_bytes());
        }
        KvReply::Unit => {
            out.push(ANSWER_UNIT);
            out.extend_from_slice(&id.to_le_bytes());
        }
        KvReply::Exists(exists) => {
            out.push(ANSWER_EXISTS);
            out.extend_from_slice(&id.to_le_bytes());
            out.push(u8::from(exists));
        }
    });
}

/// Whether `env` fits the hot layout's lengths.
fn envelope_fits(env: &Envelope) -> bool {
    u16::try_from(env.content_type.len()).is_ok()
        && env
            .schema
            .as_ref()
            .is_none_or(|s| u16::try_from(s.len()).is_ok_and(|n| n != NO_SCHEMA))
        && env.body.len() + env.content_type.len() + 64 <= MAX_FRAME
}

fn write_envelope(out: &mut Vec<u8>, env: &Envelope) {
    let len16 = |n: usize| u16::try_from(n).unwrap_or(NO_SCHEMA).to_le_bytes();
    out.extend_from_slice(&len16(env.content_type.len()));
    out.extend_from_slice(env.content_type.as_bytes());
    match &env.schema {
        Some(schema) => {
            out.extend_from_slice(&len16(schema.len()));
            out.extend_from_slice(schema.as_bytes());
        }
        None => out.extend_from_slice(&NO_SCHEMA.to_le_bytes()),
    }
    let body = u32::try_from(env.body.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&body.to_le_bytes());
    out.extend_from_slice(&env.body);
}

fn write_pid(out: &mut Vec<u8>, pid: Pid) {
    out.extend_from_slice(&pid.node.to_le_bytes());
    out.extend_from_slice(&pid.id.to_le_bytes());
    out.extend_from_slice(&pid.incarnation.to_le_bytes());
}

/// Append `frame` to `out` as a hot frame, if it is one of those the hot
/// layouts cover; `None`, writing nothing, otherwise: send it as CBOR.
#[must_use]
pub fn write_from_app(out: &mut Vec<u8>, frame: &FromApp) -> Option<()> {
    let fits = match frame {
        FromApp::Done | FromApp::Reply(None) => true,
        FromApp::Reply(Some(msg))
        | FromApp::Request {
            request:
                Request::Cast { msg, .. }
                | Request::CastWait { msg, .. }
                | Request::Call { msg, .. }
                | Request::Reply { msg, .. },
            ..
        } => envelope_fits(msg),
        _ => false,
    };
    if !fits {
        return None;
    }
    frame_into(out, |out| match frame {
        FromApp::Done => out.push(DONE),
        FromApp::Reply(None) => out.push(REPLY_NONE),
        FromApp::Reply(Some(msg)) => {
            out.push(REPLY);
            write_envelope(out, msg);
        }
        FromApp::Request {
            id,
            request: Request::Cast { to, msg },
        } => {
            out.push(CAST_TO);
            out.extend_from_slice(&id.to_le_bytes());
            write_pid(out, *to);
            write_envelope(out, msg);
        }
        FromApp::Request {
            id,
            request:
                Request::CastWait {
                    to,
                    msg,
                    timeout_ms,
                },
        } => {
            out.push(CAST_WAIT_TO);
            out.extend_from_slice(&id.to_le_bytes());
            write_pid(out, *to);
            out.extend_from_slice(&timeout_ms.to_le_bytes());
            write_envelope(out, msg);
        }
        FromApp::Request {
            id,
            request:
                Request::Call {
                    to,
                    msg,
                    timeout_ms,
                },
        } => {
            out.push(CALL_TO);
            out.extend_from_slice(&id.to_le_bytes());
            write_pid(out, *to);
            out.extend_from_slice(&timeout_ms.to_le_bytes());
            write_envelope(out, msg);
        }
        FromApp::Request {
            id,
            request: Request::Reply { call_ref, msg },
        } => {
            out.push(REPLY_TO);
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&call_ref.to_le_bytes());
            write_envelope(out, msg);
        }
        // Checked above.
        _ => {}
    });
    Some(())
}

/// Append `frame` to `out` as a hot frame, if it is one of those the hot
/// layouts cover, for an app at this crate's [`VERSION`]; `None`, writing
/// nothing, otherwise: send it as CBOR.
#[must_use]
pub fn write_to_app(out: &mut Vec<u8>, frame: &ToApp) -> Option<()> {
    write_to_app_at(out, frame, VERSION)
}

/// [`write_to_app`] for an app that speaks protocol `version`: a frame
/// whose hot layout came after that version goes as CBOR (`None`).
#[must_use]
pub fn write_to_app_at(out: &mut Vec<u8>, frame: &ToApp, version: u16) -> Option<()> {
    let fits = match frame {
        ToApp::Answer {
            answer: Answer::Unit,
            ..
        } => true,
        ToApp::Answer {
            answer: Answer::CastFailed(_),
            ..
        } => version >= CAST_LAYOUTS_SINCE,
        ToApp::Call { msg, .. }
        | ToApp::Cast { msg }
        | ToApp::Answer {
            answer: Answer::Called(Ok(msg)),
            ..
        } => envelope_fits(msg),
        _ => false,
    };
    if !fits {
        return None;
    }
    frame_into(out, |out| match frame {
        ToApp::Call { call_ref, msg } => {
            out.push(CALL);
            out.extend_from_slice(&call_ref.to_le_bytes());
            write_envelope(out, msg);
        }
        ToApp::Cast { msg } => {
            out.push(CAST);
            write_envelope(out, msg);
        }
        ToApp::Answer {
            id,
            answer: Answer::Unit,
        } => {
            out.push(ANSWER_DONE);
            out.extend_from_slice(&id.to_le_bytes());
        }
        ToApp::Answer {
            id,
            answer: Answer::Called(Ok(msg)),
        } => {
            out.push(ANSWER_CALLED);
            out.extend_from_slice(&id.to_le_bytes());
            write_envelope(out, msg);
        }
        ToApp::Answer {
            id,
            answer: Answer::CastFailed(why),
        } => {
            out.push(ANSWER_CAST_FAILED);
            out.extend_from_slice(&id.to_le_bytes());
            out.push(cast_reason(why));
        }
        // Checked above.
        _ => {}
    });
    Some(())
}

/// A hot frame from the app: a keyvalue request, borrowed, or a frame of
/// [`protocol`](crate::protocol)'s.
#[derive(Debug, Clone, PartialEq)]
pub enum FromAppHot<'a> {
    /// A keyvalue request.
    Kv(KvFrame<'a>),
    /// A done, a reply, or a cast, call or reply request.
    Frame(FromApp),
}

/// A hot frame from the node: a keyvalue answer, borrowed, or a frame of
/// [`protocol`](crate::protocol)'s.
#[derive(Debug, Clone, PartialEq)]
pub enum ToAppHot<'a> {
    /// A keyvalue request's answer: its id and the reply.
    Kv(u64, KvReply<'a>),
    /// A call, a cast, or a request's answer.
    Frame(ToApp),
}

/// Read a hot frame from the app (untrusted: every length is checked,
/// and a malformed frame is an error, never a panic).
///
/// # Errors
///
/// [`FrameError::Decode`] for an unknown tag, a truncated frame, bytes
/// left over or a string that is not UTF-8.
pub fn parse_from_app(body: &[u8]) -> Result<FromAppHot<'_>, FrameError> {
    match body.first() {
        Some(&(KV_GET | KV_SET | KV_DELETE | KV_EXISTS)) => {
            parse_kv_request(body).map(FromAppHot::Kv)
        }
        _ => {
            let mut r = Reader { rest: body };
            let frame = match r.u8()? {
                DONE => FromApp::Done,
                REPLY_NONE => FromApp::Reply(None),
                REPLY => FromApp::Reply(Some(r.envelope()?)),
                CAST_TO => {
                    let id = r.u64()?;
                    let to = r.pid()?;
                    let msg = r.envelope()?;
                    FromApp::Request {
                        id,
                        request: Request::Cast { to, msg },
                    }
                }
                CALL_TO => {
                    let id = r.u64()?;
                    let to = r.pid()?;
                    let timeout_ms = r.u32()?;
                    let msg = r.envelope()?;
                    FromApp::Request {
                        id,
                        request: Request::Call {
                            to,
                            msg,
                            timeout_ms,
                        },
                    }
                }
                CAST_WAIT_TO => {
                    let id = r.u64()?;
                    let to = r.pid()?;
                    let timeout_ms = r.u32()?;
                    let msg = r.envelope()?;
                    FromApp::Request {
                        id,
                        request: Request::CastWait {
                            to,
                            msg,
                            timeout_ms,
                        },
                    }
                }
                REPLY_TO => {
                    let id = r.u64()?;
                    let call_ref = r.u64()?;
                    let msg = r.envelope()?;
                    FromApp::Request {
                        id,
                        request: Request::Reply { call_ref, msg },
                    }
                }
                _ => return Err(malformed("unknown hot frame from the app")),
            };
            r.end()?;
            Ok(FromAppHot::Frame(frame))
        }
    }
}

/// Read a hot frame from the node.
///
/// # Errors
///
/// [`FrameError::Decode`] for an unknown tag, a truncated frame, bytes
/// left over or a string that is not UTF-8.
pub fn parse_to_app(body: &[u8]) -> Result<ToAppHot<'_>, FrameError> {
    match body.first() {
        Some(&(ANSWER_VALUE | ANSWER_NONE | ANSWER_UNIT | ANSWER_EXISTS)) => {
            parse_kv_answer(body).map(|(id, reply)| ToAppHot::Kv(id, reply))
        }
        _ => {
            let mut r = Reader { rest: body };
            let frame = match r.u8()? {
                CALL => {
                    let call_ref = r.u64()?;
                    let msg = r.envelope()?;
                    ToApp::Call { call_ref, msg }
                }
                CAST => ToApp::Cast { msg: r.envelope()? },
                ANSWER_DONE => ToApp::Answer {
                    id: r.u64()?,
                    answer: Answer::Unit,
                },
                ANSWER_CALLED => {
                    let id = r.u64()?;
                    let msg = r.envelope()?;
                    ToApp::Answer {
                        id,
                        answer: Answer::Called(Ok(msg)),
                    }
                }
                ANSWER_CAST_FAILED => ToApp::Answer {
                    id: r.u64()?,
                    answer: Answer::CastFailed(cast_error(r.u8()?)?),
                },
                _ => return Err(malformed("unknown hot frame from the node")),
            };
            r.end()?;
            Ok(ToAppHot::Frame(frame))
        }
    }
}

/// Reads a frame's body front to back; every read checks the length left.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FrameError> {
        if self.rest.len() < n {
            return Err(malformed("truncated hot frame"));
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], FrameError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, FrameError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, FrameError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, FrameError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, FrameError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn string(&mut self, len: usize) -> Result<String, FrameError> {
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| malformed("string not UTF-8"))
    }

    fn envelope(&mut self) -> Result<Envelope, FrameError> {
        let content_type = {
            let len = usize::from(self.u16()?);
            self.string(len)?
        };
        let schema = match self.u16()? {
            NO_SCHEMA => None,
            len => Some(self.string(usize::from(len))?),
        };
        let len = usize::try_from(self.u32()?).unwrap_or(usize::MAX);
        let body = self.take(len)?.to_vec();
        Ok(Envelope {
            content_type,
            schema,
            body,
        })
    }

    fn pid(&mut self) -> Result<Pid, FrameError> {
        Ok(Pid {
            node: self.u32()?,
            id: self.u64()?,
            incarnation: self.u32()?,
        })
    }

    fn end(self) -> Result<(), FrameError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(malformed("bytes after a hot frame"))
        }
    }
}

/// A failed cast's reason as its byte in the hot layout.
fn cast_reason(why: &CastError) -> u8 {
    match why {
        CastError::Noproc => 0,
        CastError::Noconnection => 1,
        CastError::Overloaded => 2,
        CastError::TooLarge => 3,
        CastError::AccessDenied => 4,
        CastError::Timeout => 5,
    }
}

/// [`cast_reason`]'s byte back to the reason.
fn cast_error(byte: u8) -> Result<CastError, FrameError> {
    Ok(match byte {
        0 => CastError::Noproc,
        1 => CastError::Noconnection,
        2 => CastError::Overloaded,
        3 => CastError::TooLarge,
        4 => CastError::AccessDenied,
        5 => CastError::Timeout,
        _ => return Err(malformed("unknown cast error")),
    })
}

fn malformed(why: &str) -> FrameError {
    FrameError::Decode(why.to_string())
}

/// Read a keyvalue request from a hot frame's body (the app's frame,
/// untrusted: every length is checked, and a malformed frame is an error,
/// never a panic).
///
/// # Errors
///
/// [`FrameError::Decode`] for an unknown tag, a truncated frame, bytes
/// left over or a key that is not UTF-8.
pub fn parse_kv_request(body: &[u8]) -> Result<KvFrame<'_>, FrameError> {
    let mut r = Reader { rest: body };
    let op = match r.u8()? {
        KV_GET => KvOp::Get,
        KV_SET => KvOp::Set,
        KV_DELETE => KvOp::Delete,
        KV_EXISTS => KvOp::Exists,
        _ => return Err(malformed("unknown hot frame from the app")),
    };
    let id = r.u64()?;
    let bucket = r.u32()?;
    let key_len = usize::from(r.u16()?);
    let key = std::str::from_utf8(r.take(key_len)?).map_err(|_| malformed("key not UTF-8"))?;
    let value = if op == KvOp::Set {
        let len = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
        r.take(len)?
    } else {
        &[]
    };
    r.end()?;
    Ok(KvFrame {
        id,
        bucket,
        op,
        key,
        value,
    })
}

/// Read a keyvalue answer from a hot frame's body: the request's id and
/// the reply.
///
/// # Errors
///
/// [`FrameError::Decode`] for an unknown tag, a truncated frame or bytes
/// left over.
pub fn parse_kv_answer(body: &[u8]) -> Result<(u64, KvReply<'_>), FrameError> {
    let mut r = Reader { rest: body };
    let tag = r.u8()?;
    let id = r.u64()?;
    let reply = match tag {
        ANSWER_VALUE => {
            let len = usize::try_from(r.u32()?).unwrap_or(usize::MAX);
            KvReply::Value(Some(r.take(len)?))
        }
        ANSWER_NONE => KvReply::Value(None),
        ANSWER_UNIT => KvReply::Unit,
        ANSWER_EXISTS => match r.u8()? {
            0 => KvReply::Exists(false),
            1 => KvReply::Exists(true),
            _ => return Err(malformed("exists is neither 0 nor 1")),
        },
        _ => return Err(malformed("unknown hot frame from the node")),
    };
    r.end()?;
    Ok((id, reply))
}

#[cfg(test)]
#[path = "hot_tests.rs"]
mod hot_tests;
