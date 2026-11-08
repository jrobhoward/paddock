//! Test fixture: keyvalue loops timed inside the app, for `bulkhead-host`'s
//! `kv_cost` benchmark, which runs the same loops as
//! `scripts/beam_ets.escript` does against ETS.
//!
//! Text calls; each loop replies the nanoseconds it took, read from the
//! app's monotonic clock before and after:
//! - `fill:<bucket>:<prefix>:<n>:<size>`: keys `<prefix>000000` to
//!   `<prefix><n-1>` (six digits), each a value of `<size>` bytes, in
//!   batches of up to 1,000 keys and about 4 MiB; replies `ok`.
//! - `loop:<bucket>:<op>:<n>:<size>:<keys>:<seed>`: `<n>` operations on
//!   keys `k000000` to `k<keys-1>`, picked by a linear congruential
//!   generator from `<seed>` (the escript's sequence). The keys and the
//!   read-or-write choices are made before the clock starts, as the
//!   escript makes them, so the loop times only the operations:
//!   - `get`: read a key; `miss`: read `m<i>`, never written.
//!   - `set`: overwrite a key with `<size>` bytes.
//!   - `insert`: write `n000000` upward (new keys), then `delete`: remove
//!     them in the same order (two loops: `insert`, then `delete`).
//!   - `incr`: increment the counter `c`.
//!   - `range`: a scan of 100 entries from a key (`<n>` scans).
//!   - `all`: a scan of the whole bucket (`<n>` scans).
//!   - `walk`: `next` from the first key, `<n>` steps, wrapping.
//!   - `mix95`, `mix50`: reads and overwrites of keys, 95 or 50 in 100
//!     reads; `hot`: overwrite `k000000` every time.
//!   - `many10`, `many100`: the `<n>` keys read in `get_many` batches of 10
//!     or 100 (the time is still per key).
//!   - `chain`: `<n>` dependent reads along the chain `fillchain` wrote,
//!     from `c<seed mod keys>`: each value is the next key, as in a walk
//!     of a trie or a graph.
//! - `fillchain:<bucket>:<n>`: keys `c000000` to `c<n-1>`, each holding the
//!   next key of the chain `i -> (7919 i + 1) mod n`; replies `ok`.

use std::time::Instant;

use bulkhead_sdk::kv::{Bucket, KvError};
use bulkhead_sdk::watch;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};

struct KvBench;

/// The escript's generator: the same multiplier and increment as the
/// chaos fixture's `busy`, the high bits for the index.
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

fn key(prefix: &str, i: u64) -> String {
    format!("{prefix}{i:06}")
}

fn number(s: Option<&&str>) -> u64 {
    s.and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn fill(f: &[&str]) -> Result<String, KvError> {
    let bucket = Bucket::open(f.first().copied().unwrap_or(""))?;
    let prefix = f.get(1).copied().unwrap_or("k");
    let (n, size) = (number(f.get(2)), number(f.get(3)));
    let value = vec![7u8; usize::try_from(size).unwrap_or(0)];
    // Up to 1,000 keys and about 4 MiB a batch: a native app's request is
    // one frame, at most 16 MiB.
    let per_batch = (4 << 20) / size.max(1);
    let per_batch = per_batch.clamp(1, 1000);
    let mut i = 0;
    while i < n {
        let batch: Vec<(String, Vec<u8>)> = (i..n.min(i + per_batch))
            .map(|j| (key(prefix, j), value.clone()))
            .collect();
        i += batch.len() as u64;
        bucket.set_many(&batch)?;
    }
    Ok("ok".into())
}

fn fill_chain(f: &[&str]) -> Result<String, KvError> {
    let bucket = Bucket::open(f.first().copied().unwrap_or(""))?;
    let n = number(f.get(1)).max(1);
    let mut i = 0;
    while i < n {
        let batch: Vec<(String, Vec<u8>)> = (i..n.min(i + 1000))
            .map(|j| (key("c", j), key("c", (7919 * j + 1) % n).into_bytes()))
            .collect();
        i += batch.len() as u64;
        bucket.set_many(&batch)?;
    }
    Ok("ok".into())
}

/// Run one loop; the nanoseconds it took. Errors are counted by the
/// caller from the reply.
#[allow(clippy::too_many_lines)] // one arm per operation, as listed above
fn run(f: &[&str]) -> Result<u128, String> {
    let name = f.first().copied().unwrap_or("");
    let op = f.get(1).copied().unwrap_or("");
    let (n, size, keys) = (number(f.get(2)), number(f.get(3)), number(f.get(4)));
    let mut rng = Lcg(number(f.get(5)));
    let bucket = Bucket::open(name).map_err(|e| e.to_string())?;
    let value = vec![7u8; usize::try_from(size).unwrap_or(0)];
    let e = |e: KvError| e.to_string();
    let w = |e: watch::WatchError| e.to_string();
    let reads = match op {
        "mix95" => 95,
        "mix50" => 50,
        _ => 100,
    };
    let picked: Vec<(String, bool)> = (0..n)
        .map(|i| match op {
            "miss" => (key("m", rng.next(keys)), true),
            "insert" | "delete" => (key("n", i), false),
            "hot" => (key("k", 0), false),
            _ => {
                let k = key("k", rng.next(keys));
                (k, rng.next(100) < reads)
            }
        })
        .collect();
    let started = Instant::now();
    match op {
        "get" | "miss" => {
            for (k, _) in &picked {
                std::hint::black_box(bucket.get(k).map_err(e)?);
            }
        }
        "set" | "insert" | "hot" => {
            for (k, _) in &picked {
                bucket.set(k, &value).map_err(e)?;
            }
        }
        "delete" => {
            for (k, _) in &picked {
                bucket.delete(k).map_err(e)?;
            }
        }
        "incr" => {
            for _ in 0..n {
                std::hint::black_box(bucket.increment("c", 1).map_err(e)?);
            }
        }
        "range" => {
            for (from, _) in &picked {
                let page = watch::scan(name)
                    .start(from)
                    .limit(100)
                    .page(None)
                    .map_err(w)?;
                std::hint::black_box(page);
            }
        }
        "all" => {
            for _ in 0..n {
                for entry in watch::scan(name).prefix("k").entries() {
                    std::hint::black_box(entry.map_err(w)?);
                }
            }
        }
        "walk" => {
            let mut at = watch::first(name).map_err(w)?;
            for _ in 0..n {
                at = match at {
                    Some(entry) => watch::next(name, &entry.key).map_err(w)?,
                    None => watch::first(name).map_err(w)?,
                };
                std::hint::black_box(&at);
            }
        }
        "many10" | "many100" => {
            let batch = if op == "many10" { 10 } else { 100 };
            let keys: Vec<String> = picked.iter().map(|(k, _)| k.clone()).collect();
            let started = Instant::now();
            for chunk in keys.chunks(batch) {
                std::hint::black_box(bucket.get_many(chunk).map_err(e)?);
            }
            return Ok(started.elapsed().as_nanos());
        }
        "chain" => {
            let mut at = key("c", number(f.get(5)) % keys.max(1));
            for _ in 0..n {
                let next = bucket.get(&at).map_err(e)?.unwrap_or_default();
                at = String::from_utf8(next).map_err(|e| e.to_string())?;
            }
            std::hint::black_box(at);
        }
        "mix95" | "mix50" => {
            for (k, read) in &picked {
                if *read {
                    std::hint::black_box(bucket.get(k).map_err(e)?);
                } else {
                    bucket.set(k, &value).map_err(e)?;
                }
            }
        }
        other => return Err(format!("unknown op {other}")),
    }
    Ok(started.elapsed().as_nanos())
}

impl App for KvBench {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self)
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let (verb, rest) = text.split_once(':').unwrap_or((text, ""));
        let f: Vec<&str> = rest.split(':').collect();
        let reply = match verb {
            "fill" => fill(&f).unwrap_or_else(|e| format!("err:{e}")),
            "fillchain" => fill_chain(&f).unwrap_or_else(|e| format!("err:{e}")),
            "loop" => run(&f).map_or_else(|e| format!("err:{e}"), |ns| ns.to_string()),
            _ => format!("err:unknown command {verb}"),
        };
        Reply::Now(Envelope::text(reply))
    }
}

bulkhead_sdk::app!(KvBench);
