//! A published bucket's snapshot (ROADMAP T4): a shared bucket's entries
//! at one revision, which the node writes into a sealed memfd and passes
//! to every native instance that reads the bucket. The app maps it
//! read-only and answers `get`, `get_many` and `exists` from it without
//! asking the node (spike S31).
//!
//! Layout, little endian:
//!
//! | Offset | Field |
//! | --- | --- |
//! | 0 | [`MAGIC`] (8 bytes) |
//! | 8 | revision (u64): every write at or below it is in the snapshot |
//! | 16 | entries (u64) |
//! | 24 | slots (u64, a power of two) |
//! | 32 | the index's offset (u64, a multiple of 8) |
//! | 40 to 64 | zero |
//!
//! From 64, the entries back to back: the key's length (u32), the
//! value's (u32), the key, the value. At the index's offset, `slots` u64s,
//! each 0 (empty) or an entry's offset plus 1: open addressing by
//! [`hash`] of the key, probing the next slot (wrapping) on a collision.
//!
//! A snapshot may hold writes after its revision too: it is read a page
//! at a time, so a key changed during the read can be newer. Reading it
//! checks every offset and length, so a snapshot that breaks these rules
//! answers `None`, never out of bounds.

/// The first eight bytes of a snapshot: the format and its version.
pub const MAGIC: [u8; 8] = *b"BHSNAP\0\x01";

const HEADER: usize = 64;

/// The key's hash: 8-byte words folded with a multiply and rotate.
#[must_use]
pub fn hash(key: &[u8]) -> u64 {
    let mut h = key.len() as u64;
    for chunk in key.chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        h = (h.rotate_left(5) ^ u64::from_le_bytes(word)).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    h ^ (h >> 29)
}

/// The snapshot of `entries` (keys distinct) at `revision`.
///
/// # Panics
///
/// Never for keys and values under 4 GiB each, which a bucket's quota
/// keeps them below; one over that limit is cut short.
#[must_use]
pub fn build(revision: u64, entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let slots = (entries.len() * 2).max(8).next_power_of_two();
    let data: usize = entries.iter().map(|(k, v)| 8 + k.len() + v.len()).sum();
    let index = (HEADER + data).next_multiple_of(8);
    let mut out = vec![0u8; index + slots * 8];
    out[..8].copy_from_slice(&MAGIC);
    for (at, word) in [
        (8, revision),
        (16, entries.len() as u64),
        (24, slots as u64),
        (32, index as u64),
    ] {
        out[at..at + 8].copy_from_slice(&word.to_le_bytes());
    }
    let mut at = HEADER;
    for (key, value) in entries {
        let (k, v) = (key.as_bytes(), value.as_slice());
        let lens = [k.len(), v.len()].map(|n| u32::try_from(n).unwrap_or(u32::MAX));
        out[at..at + 4].copy_from_slice(&lens[0].to_le_bytes());
        out[at + 4..at + 8].copy_from_slice(&lens[1].to_le_bytes());
        out[at + 8..at + 8 + k.len()].copy_from_slice(k);
        out[at + 8 + k.len()..at + 8 + k.len() + v.len()].copy_from_slice(v);
        let mut slot = usize::try_from(hash(k)).unwrap_or(0) & (slots - 1);
        loop {
            let s = index + slot * 8;
            if out[s..s + 8] == [0; 8] {
                out[s..s + 8].copy_from_slice(&(at as u64 + 1).to_le_bytes());
                break;
            }
            slot = (slot + 1) & (slots - 1);
        }
        at += 8 + k.len() + v.len();
    }
    out
}

/// A snapshot's bytes, its header checked.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<'a> {
    bytes: &'a [u8],
    revision: u64,
    entries: u64,
    slots: usize,
    index: usize,
}

fn word(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn half(bytes: &[u8], at: usize) -> Option<usize> {
    let w: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    usize::try_from(u32::from_le_bytes(w)).ok()
}

impl<'a> Snapshot<'a> {
    /// Read `bytes`' header.
    ///
    /// # Errors
    ///
    /// What is wrong with it: not a snapshot, or an index that does not
    /// fit.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, &'static str> {
        if bytes.get(..8) != Some(&MAGIC[..]) {
            return Err("not a bucket snapshot");
        }
        let field = |at| word(bytes, at).ok_or("a short header");
        let (revision, entries, slots, index) = (field(8)?, field(16)?, field(24)?, field(32)?);
        let slots = usize::try_from(slots).map_err(|_| "too many slots")?;
        let index = usize::try_from(index).map_err(|_| "an index past the end")?;
        let fits = slots
            .checked_mul(8)
            .and_then(|n| n.checked_add(index))
            .is_some_and(|end| end <= bytes.len());
        if !slots.is_power_of_two() || index < HEADER || !fits {
            return Err("an index that does not fit");
        }
        Ok(Self {
            bytes,
            revision,
            entries,
            slots,
            index,
        })
    }

    /// Every write at or below this revision is in the snapshot.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// How many entries the snapshot holds.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.entries
    }

    /// Whether the snapshot holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// The value of `key`, if the snapshot holds it.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&'a [u8]> {
        let bytes = self.bytes;
        let mut slot = usize::try_from(hash(key)).unwrap_or(0) & (self.slots - 1);
        for _ in 0..self.slots {
            let entry = word(bytes, self.index + slot * 8)?;
            if entry == 0 {
                return None;
            }
            let at = usize::try_from(entry - 1).ok()?;
            let (k, v) = (half(bytes, at)?, half(bytes, at + 4)?);
            let key_at = at.checked_add(8)?;
            let value_at = key_at.checked_add(k)?;
            if bytes.get(key_at..value_at)? == key {
                return bytes.get(value_at..value_at.checked_add(v)?);
            }
            slot = (slot + 1) & (self.slots - 1);
        }
        None
    }
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod snapshot_tests;
