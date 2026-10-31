//! Writes the reference table the guest embeds: a 16-byte header (records,
//! version, hash slots, all `u32` little-endian), `S27_ENTRIES` records
//! (default 100,000), each a 16-byte key (`k` and seven digits, zero
//! padded) and a 160-byte value whose first 4 bytes name another record,
//! sorted by key; then a hash index, a power of two of `u32` record numbers
//! (at least twice the records, `u32::MAX` empty, linear probing).
//! `S27_VERSION` (default 1) changes every value, so two builds differ in
//! data.
use std::io::Write;

const KEY: usize = 16;
const VALUE: usize = 160;

fn main() {
    println!("cargo::rerun-if-env-changed=S27_ENTRIES");
    println!("cargo::rerun-if-env-changed=S27_VERSION");
    let n: u32 = std::env::var("S27_ENTRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let version: u8 = std::env::var("S27_VERSION")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("table.bin");
    let mut f = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
    let slots = (n as usize * 2).next_power_of_two();
    f.write_all(&n.to_le_bytes()).unwrap();
    f.write_all(&u32::from(version).to_le_bytes()).unwrap();
    f.write_all(&(slots as u32).to_le_bytes()).unwrap();
    f.write_all(&[0; 4]).unwrap();
    let mut index = vec![u32::MAX; slots];
    for i in 0..n {
        let mut key = [0u8; KEY];
        let k = format!("k{i:07}");
        key[..k.len()].copy_from_slice(k.as_bytes());
        let mut at = hash(&key, slots);
        while index[at] != u32::MAX {
            at = (at + 1) & (slots - 1);
        }
        index[at] = i;
        f.write_all(&key).unwrap();
        let mut value = [version; VALUE];
        let next = ((u64::from(i) * 7919 + 1) % u64::from(n)) as u32;
        value[..4].copy_from_slice(&next.to_le_bytes());
        f.write_all(&value).unwrap();
    }
    for slot in index {
        f.write_all(&slot.to_le_bytes()).unwrap();
    }
}

/// The slot `key` hashes to; the guest's `hash` is the same.
fn hash(key: &[u8; KEY], slots: usize) -> usize {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let a = u64::from_le_bytes(key[..8].try_into().unwrap());
    let b = u64::from_le_bytes(key[8..].try_into().unwrap());
    let x = a.wrapping_mul(K).rotate_left(29) ^ b;
    (x.wrapping_mul(K) >> (64 - slots.trailing_zeros())) as usize
}
