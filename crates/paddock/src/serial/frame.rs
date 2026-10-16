//! Heartbeat frames on a serial line.
//!
//! A frame is `P D 1` (magic and version), the sender's node id and the
//! receiver's (big-endian `u32`s), the payload's length (`u16`), the
//! payload, and a CRC-16/CCITT-FALSE of everything before it. It is
//! COBS-encoded, so it holds no zero byte, and ends with one. A reader that
//! starts mid-frame, or meets line noise, loses at most the frame it is in:
//! the next zero starts afresh, and a frame whose CRC, magic or length is
//! wrong is dropped.

/// Magic and version.
const MAGIC: [u8; 3] = *b"PD1";
/// Bytes before the payload: magic, from, to, length.
const HEAD: usize = 3 + 4 + 4 + 2;
/// The longest payload a frame carries. Failover's beat payloads are tens
/// of bytes.
pub const MAX_PAYLOAD: usize = 1024;
/// The longest encoded frame the decoder keeps, the delimiter excluded.
const MAX_ENCODED: usize = (HEAD + MAX_PAYLOAD + 2) * 255 / 254 + 2;

/// A decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The sending node.
    pub from: u32,
    /// The node it is for.
    pub to: u32,
    /// The beat payload.
    pub payload: Vec<u8>,
}

/// Encode a frame; `None` if `payload` is over [`MAX_PAYLOAD`].
#[must_use]
pub fn encode(from: u32, to: u32, payload: &[u8]) -> Option<Vec<u8>> {
    let len = u16::try_from(payload.len())
        .ok()
        .filter(|_| payload.len() <= MAX_PAYLOAD)?;
    let mut raw = Vec::with_capacity(HEAD + payload.len() + 2);
    raw.extend_from_slice(&MAGIC);
    raw.extend_from_slice(&from.to_be_bytes());
    raw.extend_from_slice(&to.to_be_bytes());
    raw.extend_from_slice(&len.to_be_bytes());
    raw.extend_from_slice(payload);
    let crc = crc16(&raw);
    raw.extend_from_slice(&crc.to_be_bytes());
    let mut out = cobs_encode(&raw);
    out.push(0);
    Some(out)
}

/// Splits a byte stream into frames.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Bytes are being dropped until the next zero (a frame too long).
    skipping: bool,
}

impl Decoder {
    /// Feed `bytes`; the valid frames they complete, in order.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        for &b in bytes {
            if b == 0 {
                if !self.skipping
                    && let Some(f) = decode(&self.buf)
                {
                    frames.push(f);
                }
                self.buf.clear();
                self.skipping = false;
            } else if self.buf.len() >= MAX_ENCODED {
                self.buf.clear();
                self.skipping = true;
            } else if !self.skipping {
                self.buf.push(b);
            }
        }
        frames
    }
}

/// One encoded frame, without its delimiter.
fn decode(encoded: &[u8]) -> Option<Frame> {
    let raw = cobs_decode(encoded)?;
    if raw.len() < HEAD + 2 || raw[..3] != MAGIC {
        return None;
    }
    let (body, crc) = raw.split_at(raw.len() - 2);
    if crc16(body).to_be_bytes() != crc {
        return None;
    }
    let u32_at = |i: usize| u32::from_be_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
    let len = usize::from(u16::from_be_bytes([body[11], body[12]]));
    if body.len() != HEAD + len || len > MAX_PAYLOAD {
        return None;
    }
    Some(Frame {
        from: u32_at(3),
        to: u32_at(7),
        payload: body[HEAD..].to_vec(),
    })
}

/// CRC-16/CCITT-FALSE: polynomial 0x1021, initial 0xFFFF, no reflection.
pub(crate) fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x1021
            };
        }
    }
    crc
}

/// Consistent overhead byte stuffing: `raw` with no zero bytes.
pub(crate) fn cobs_encode(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 254 + 1);
    let mut code_at = 0;
    out.push(0);
    let mut code: u8 = 1;
    for &b in raw {
        if b == 0 {
            out[code_at] = code;
            code_at = out.len();
            out.push(0);
            code = 1;
        } else {
            out.push(b);
            code += 1;
            if code == 0xFF {
                out[code_at] = code;
                code_at = out.len();
                out.push(0);
                code = 1;
            }
        }
    }
    out[code_at] = code;
    out
}

/// The inverse of [`cobs_encode`]; `None` for input it cannot have made.
pub(crate) fn cobs_decode(encoded: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(encoded.len());
    let mut i = 0;
    while i < encoded.len() {
        let code = usize::from(encoded[i]);
        if code == 0 || i + code > encoded.len() {
            return None;
        }
        out.extend_from_slice(&encoded[i + 1..i + code]);
        i += code;
        if code < 0xFF && i < encoded.len() {
            out.push(0);
        }
    }
    Some(out)
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod frame_tests;
