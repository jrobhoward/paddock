//! Spike S27's guest: a read-only table built into the component's data
//! segment (`include_bytes!`), read in place.
//!
//! Text calls:
//! - `get:<n>:<seed>`: `<n>` lookups of keys picked by the kvbench
//!   generator, each a binary search copying the value's first 16 bytes
//!   out (the size the ETS and `persistent_term` runs read); replies the
//!   nanoseconds the loop took.
//! - `chain:<n>`: `<n>` dependent lookups, each value naming the next
//!   record; replies the nanoseconds.
//! - `hget:<n>:<seed>`, `hchain:<n>`: the same through the hash index.
//! - `touch`: read one byte of every 4 KiB page of the table; replies
//!   their sum, so every page is resident.
//! - `info`: `<records> <bytes> <version>`.
//! - `count`: add one to a counter and reply it (state for an upgrade).

use std::time::Instant;

use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};

const KEY: usize = 16;
const RECORD: usize = KEY + 160;

static TABLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/table.bin"));

fn header(i: usize) -> u32 {
    u32::from_le_bytes(TABLE[i * 4..i * 4 + 4].try_into().unwrap())
}

fn records() -> u32 {
    header(0)
}

fn record(i: usize) -> &'static [u8] {
    let at = 16 + i * RECORD;
    &TABLE[at..at + RECORD]
}

/// The slot `key` hashes to, as `build.rs` placed it.
fn hash(key: &[u8; KEY], slots: usize) -> usize {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let a = u64::from_le_bytes(key[..8].try_into().unwrap());
    let b = u64::from_le_bytes(key[8..].try_into().unwrap());
    let x = a.wrapping_mul(K).rotate_left(29) ^ b;
    (x.wrapping_mul(K) >> (64 - slots.trailing_zeros())) as usize
}

/// The record with `key`, through the hash index.
fn hfind(key: &[u8; KEY]) -> Option<&'static [u8]> {
    let slots = header(2) as usize;
    let index = 16 + records() as usize * RECORD;
    let mut at = hash(key, slots);
    loop {
        let o = index + at * 4;
        let i = u32::from_le_bytes(TABLE[o..o + 4].try_into().unwrap());
        if i == u32::MAX {
            return None;
        }
        let r = record(i as usize);
        if r[..KEY] == key[..] {
            return Some(&r[KEY..]);
        }
        at = (at + 1) & (slots - 1);
    }
}

/// The record with `key`, by binary search.
fn find(key: &[u8; KEY]) -> Option<&'static [u8]> {
    let (mut lo, mut hi) = (0usize, records() as usize);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let r = record(mid);
        match r[..KEY].cmp(&key[..]) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Some(&r[KEY..]),
        }
    }
    None
}

/// `k` and `i` in seven digits, as `build.rs` writes it.
fn key(mut i: u64) -> [u8; KEY] {
    let mut k = [0u8; KEY];
    k[0] = b'k';
    for d in (1..=7).rev() {
        k[d] = b'0' + (i % 10) as u8;
        i /= 10;
    }
    k
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below.max(1)
    }
}

#[derive(Default)]
struct Reference {
    count: u64,
}

fn number(s: Option<&str>) -> u64 {
    s.and_then(|s| s.parse().ok()).unwrap_or(0)
}

impl App for Reference {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self::default())
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("").to_string();
        let mut f = text.split(':');
        let op = f.next().unwrap_or("");
        let find: fn(&[u8; KEY]) -> Option<&'static [u8]> =
            if op.starts_with('h') { hfind } else { find };
        let reply = match op {
            "get" | "hget" => {
                let n = number(f.next());
                let mut rng = Lcg(number(f.next()));
                let keys: Vec<[u8; KEY]> = (0..n)
                    .map(|_| key(rng.next(u64::from(records()))))
                    .collect();
                let mut out = [0u8; 16];
                let started = Instant::now();
                for k in &keys {
                    let v = find(k).expect("every key is in the table");
                    out.copy_from_slice(&v[..16]);
                    std::hint::black_box(&out);
                }
                started.elapsed().as_nanos().to_string()
            }
            "chain" | "hchain" => {
                let n = number(f.next());
                let started = Instant::now();
                let mut at = 0u64;
                for _ in 0..n {
                    let v = find(&key(at)).expect("in the table");
                    at = u64::from(u32::from_le_bytes(v[..4].try_into().unwrap()));
                }
                std::hint::black_box(at);
                started.elapsed().as_nanos().to_string()
            }
            "touch" => {
                let sum: u64 = TABLE.iter().step_by(4096).map(|&b| u64::from(b)).sum();
                sum.to_string()
            }
            "info" => format!("{} {} {}", records(), TABLE.len(), header(1)),
            "count" => {
                self.count += 1;
                self.count.to_string()
            }
            other => format!("unknown {other}"),
        };
        Reply::Now(Envelope::text(reply))
    }

    fn save_snapshot(&self) -> Result<Vec<u8>, String> {
        Ok(self.count.to_le_bytes().to_vec())
    }

    fn load_snapshot(_ctx: &mut Ctx, _from: &str, snapshot: &[u8]) -> Result<Self, String> {
        let count = snapshot
            .get(..8)
            .and_then(|b| b.try_into().ok())
            .map_or(0, u64::from_le_bytes);
        Ok(Self { count })
    }
}

paddock_sdk::app!(Reference);
