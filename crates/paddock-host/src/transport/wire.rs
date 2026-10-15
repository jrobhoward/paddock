//! The wire format between nodes (ROADMAP D1, D3): a [`Packet`] as CBOR,
//! with the protocol version alongside.
//!
//! A connection starts with a [`Hello`] each way, carrying the versions the
//! sender speaks; both use the highest version both speak ([`agree`]), or
//! close the connection. Today there is one, [`VERSION`]. Every later frame
//! on the connection is `[version, packet]` in CBOR, so a node can still
//! tell which version a frame was written in. Message bodies and heartbeat
//! payloads are CBOR byte strings.

use serde::{Deserialize, Serialize};

use super::Packet;

/// The protocol version this node writes. Version 2 acknowledges unlinks
/// (`Unlink` carries an id, `UnlinkAck` answers it); version 3 stamps
/// casts, calls, exit signals and links with the sender's app and routes
/// for messaging ACLs.
pub const VERSION: u16 = 3;

/// The oldest protocol version this node reads. Earlier versions carry no
/// sender stamp, without which messaging ACLs cannot be checked, so they
/// are not spoken.
pub const MIN_VERSION: u16 = 3;

/// The first message each side of a connection sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The oldest version the sender speaks.
    pub min_version: u16,
    /// The newest version the sender speaks.
    pub max_version: u16,
    /// The sender's node id; its certificate must name it.
    pub node: u32,
    /// The sender's creation.
    pub creation: u32,
}

impl Hello {
    /// This node's hello.
    #[must_use]
    pub fn new(node: u32, creation: u32) -> Self {
        Self {
            min_version: MIN_VERSION,
            max_version: VERSION,
            node,
            creation,
        }
    }
}

/// The version two hellos agree on: the highest both speak, if any.
#[must_use]
pub fn agree(mine: &Hello, theirs: &Hello) -> Option<u16> {
    let high = mine.max_version.min(theirs.max_version);
    let low = mine.min_version.max(theirs.min_version);
    (high >= low).then_some(high)
}

/// Why bytes are not a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wire: {}", self.0)
    }
}

impl std::error::Error for WireError {}

/// Encode any wire value as CBOR.
///
/// # Errors
///
/// If the value cannot be encoded (it can always be, for wire types).
pub fn to_cbor<T: Serialize>(value: &T) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).map_err(|e| WireError(e.to_string()))?;
    Ok(out)
}

/// Decode a wire value from CBOR.
///
/// # Errors
///
/// If `bytes` are not a CBOR encoding of `T`.
pub fn from_cbor<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, WireError> {
    ciborium::from_reader(bytes).map_err(|e| WireError(e.to_string()))
}

/// `packet` as written on a connection at `version`.
///
/// # Errors
///
/// As for [`to_cbor`].
pub fn encode(version: u16, packet: &Packet) -> Result<Vec<u8>, WireError> {
    to_cbor(&(version, packet))
}

/// A packet read from a connection that agreed on `version`.
///
/// # Errors
///
/// Bytes that are not a packet, or one written at another version.
pub fn decode(version: u16, bytes: &[u8]) -> Result<Packet, WireError> {
    let (written, packet): (u16, Packet) = from_cbor(bytes)?;
    if written != version {
        return Err(WireError(format!(
            "frame at version {written} on a version {version} connection"
        )));
    }
    Ok(packet)
}

/// `Vec<u8>` as a CBOR byte string, not an array of integers.
pub(crate) mod bytes {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
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

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        d.deserialize_byte_buf(Bytes)
    }
}

/// [`crate::failover::FailoverMsg`] on the wire, through a mirror type so
/// the failover module carries no wire concerns.
pub(crate) mod failover {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::failover::FailoverMsg;

    #[derive(Serialize, Deserialize)]
    enum Wire {
        Renew {
            seq: u64,
            epoch: u64,
        },
        Request {
            seq: u64,
            resume: bool,
            epoch: u64,
            confirmed_ago_ms: u64,
        },
        LeaseOk {
            seq: u64,
        },
        Fenced {
            epoch: u64,
        },
        Granted {
            seq: u64,
            epoch: u64,
        },
        NotYet {
            wait_ms: u64,
        },
        Refused {
            epoch: u64,
        },
    }

    pub(crate) fn serialize<S: Serializer>(m: &FailoverMsg, s: S) -> Result<S::Ok, S::Error> {
        let wire = match *m {
            FailoverMsg::Renew { seq, epoch } => Wire::Renew { seq, epoch },
            FailoverMsg::Request {
                seq,
                resume,
                epoch,
                confirmed_ago_ms,
            } => Wire::Request {
                seq,
                resume,
                epoch,
                confirmed_ago_ms,
            },
            FailoverMsg::LeaseOk { seq } => Wire::LeaseOk { seq },
            FailoverMsg::Fenced { epoch } => Wire::Fenced { epoch },
            FailoverMsg::Granted { seq, epoch } => Wire::Granted { seq, epoch },
            FailoverMsg::NotYet { wait_ms } => Wire::NotYet { wait_ms },
            FailoverMsg::Refused { epoch } => Wire::Refused { epoch },
        };
        wire.serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<FailoverMsg, D::Error> {
        Ok(match Wire::deserialize(d)? {
            Wire::Renew { seq, epoch } => FailoverMsg::Renew { seq, epoch },
            Wire::Request {
                seq,
                resume,
                epoch,
                confirmed_ago_ms,
            } => FailoverMsg::Request {
                seq,
                resume,
                epoch,
                confirmed_ago_ms,
            },
            Wire::LeaseOk { seq } => FailoverMsg::LeaseOk { seq },
            Wire::Fenced { epoch } => FailoverMsg::Fenced { epoch },
            Wire::Granted { seq, epoch } => FailoverMsg::Granted { seq, epoch },
            Wire::NotYet { wait_ms } => FailoverMsg::NotYet { wait_ms },
            Wire::Refused { epoch } => FailoverMsg::Refused { epoch },
        })
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod wire_tests;
