//! The spike's frames and how they cross a ring: a 4-byte length, then
//! the body `kind (u8) | id (u64) | rest`. Fixed and tiny, so every
//! mechanism pays the same for its bytes and only the mechanism differs.

use bulkhead_native::channel::{RING_CAP, Ring};

// App to node.
/// A bucket get: rest is the key (u32); answered with a 16-byte value.
pub const GET: u8 = 1;
/// A cast to the node: counted, not answered.
pub const CAST: u8 = 2;
/// A call to another app: rest is its index (u32) and a payload.
pub const CALL: u8 = 3;
/// A reply to an `INCOMING`: rest is the caller's app and channel (u32
/// each) and the payload.
pub const REPLY: u8 = 4;
/// Start the measured part: the node takes its CPU readings. Answered.
pub const START: u8 = 5;
/// End it: the node reports its CPU. Answered.
pub const DONE: u8 = 6;
/// A busy callback ended: the node sends the next `TICK`.
pub const TICK_DONE: u8 = 7;

// Node to app.
/// An answer, matched by id.
pub const ANSWER: u8 = 10;
/// A call from another app: rest is its app and channel (u32 each) and
/// the payload.
pub const INCOMING: u8 = 11;
/// A callback that keeps the callback thread busy for the run's `busy`.
pub const TICK: u8 = 12;

/// Bytes of a value and of a call's payload.
pub const PAYLOAD: usize = 16;

/// A frame with its length prefix.
#[must_use]
pub fn frame(kind: u8, id: u64, rest: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(13 + rest.len());
    f.extend(u32::try_from(9 + rest.len()).unwrap().to_le_bytes());
    f.push(kind);
    f.extend(id.to_le_bytes());
    f.extend(rest);
    f
}

pub fn kind(body: &[u8]) -> u8 {
    body[0]
}

pub fn id(body: &[u8]) -> u64 {
    u64::from_le_bytes(body[1..9].try_into().unwrap())
}

pub fn rest(body: &[u8]) -> &[u8] {
    &body[9..]
}

pub fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Write all of `f`, calling `wake` after each part written (the reader
/// may sleep on a ring that was full).
pub fn write_frame(ring: Ring<'_>, f: &[u8], wake: impl Fn()) {
    let mut off = 0;
    while off < f.len() {
        let n = ring.write(&f[off..]).expect("corrupt ring");
        off += n;
        if n > 0 {
            wake();
        } else {
            std::thread::yield_now();
        }
    }
}

/// The next frame's body into `buf`, if one has begun; `false` if the
/// ring is empty.
pub fn read_frame(ring: Ring<'_>, buf: &mut Vec<u8>) -> bool {
    if !ring.is_readable() {
        return false;
    }
    let mut p = [0u8; 4];
    read_exact(ring, &mut p);
    let len = u32::from_le_bytes(p) as usize;
    assert!(len <= RING_CAP, "frame of {len} bytes");
    buf.resize(len, 0);
    read_exact(ring, buf);
    true
}

fn read_exact(ring: Ring<'_>, out: &mut [u8]) {
    let mut off = 0;
    while off < out.len() {
        let n = ring.read(&mut out[off..]).expect("corrupt ring");
        off += n;
        if n == 0 {
            std::hint::spin_loop();
        }
    }
}
