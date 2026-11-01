//! A read-only bucket snapshot: fixed-size records and a hash index, as
//! a node could publish into a sealed memfd for app processes to map.
//! Shared by `s31-probe` and `s31-bench` (by path).
//!
//! Layout: `n` and `slots` (u64 each), then `n` records of a 16-byte key
//! (zero padded) and a 16-byte value, then `slots` u32 record numbers
//! (`u32::MAX` empty), open addressing with linear probing.

#![allow(dead_code)] // each binary uses half

pub const RECORD: usize = 32;

pub fn hash(key: &[u8]) -> u64 {
    let mut h = 0u64;
    for chunk in key.chunks(8) {
        let mut w = [0u8; 8];
        w[..chunk.len()].copy_from_slice(chunk);
        h = (h.rotate_left(5) ^ u64::from_le_bytes(w)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    h.rotate_left(26)
}

pub fn key(i: u64) -> [u8; 16] {
    let mut k = [0u8; 16];
    let s = format!("k{i:06}");
    k[..s.len()].copy_from_slice(s.as_bytes());
    k
}

/// A snapshot of keys `k000000` .. with 16-byte values.
pub fn build(n: u64, value: impl Fn(u64) -> [u8; 16]) -> Vec<u8> {
    let slots = (n * 2).next_power_of_two();
    let records_end = 16 + n as usize * RECORD;
    let mut out = vec![0u8; records_end + slots as usize * 4];
    out[..8].copy_from_slice(&n.to_le_bytes());
    out[8..16].copy_from_slice(&slots.to_le_bytes());
    for s in 0..slots as usize {
        out[records_end + s * 4..records_end + s * 4 + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    }
    for i in 0..n {
        let at = 16 + i as usize * RECORD;
        let k = key(i);
        out[at..at + 16].copy_from_slice(&k);
        out[at + 16..at + 32].copy_from_slice(&value(i));
        let mut s = hash(&k) & (slots - 1);
        loop {
            let at = records_end + s as usize * 4;
            if out[at..at + 4] == u32::MAX.to_le_bytes() {
                out[at..at + 4].copy_from_slice(&(i as u32).to_le_bytes());
                break;
            }
            s = (s + 1) & (slots - 1);
        }
    }
    out
}

/// The value of `key` in a snapshot.
pub fn get<'a>(snap: &'a [u8], key: &[u8; 16]) -> Option<&'a [u8]> {
    let n = u64::from_le_bytes(snap[..8].try_into().unwrap()) as usize;
    let slots = u64::from_le_bytes(snap[8..16].try_into().unwrap());
    let records_end = 16 + n * RECORD;
    let mut s = hash(key) & (slots - 1);
    loop {
        let at = records_end + s as usize * 4;
        let r = u32::from_le_bytes(snap[at..at + 4].try_into().unwrap());
        if r == u32::MAX {
            return None;
        }
        let rec = 16 + r as usize * RECORD;
        if &snap[rec..rec + 16] == key {
            return Some(&snap[rec + 16..rec + 32]);
        }
        s = (s + 1) & (slots - 1);
    }
}
