//! Spike S28 guest: a mirror of a bucket kept in the instance by a scan and
//! a watch (the prototype of ROADMAP H8), and the same reads through the
//! bucket and through owner instances for comparison.
//!
//! Text calls, replies as text:
//! - `fill:<bucket>:<n>:<size>`: keys `k000000` ..., `<size>`-byte values
//!   whose first 8 bytes are the wall-clock time in ns.
//! - `fillchain:<bucket>:<n>`: keys `c000000` ..., each value the next key
//!   of `i -> (7919 i + 1) mod n`.
//! - `mirror:<bucket>:<prefix>:<mode>:<kind>`: build this instance's
//!   mirror. `<mode>` `watch`: watch from now, then scan (a change during
//!   the scan arrives as a notice); `resume`: scan, then watch from the
//!   first page's revision, rescanning on `too-old`. `<kind>` `hash` or
//!   `tree` (std's SipHash map or B-tree), or `fx` (a hash map with
//!   FxHash). Replies `<entries> <ns> <rescans> <linear memory bytes>`.
//! - `mget:<n>:<keys>:<seed>`, `mchain:<n>:<keys>:<seed>`: reads from the
//!   mirror; `bget:<bucket>:...`, `bchain:<bucket>:...` the same through
//!   `wasi:keyvalue`; `oget:<owners>:<n>:<keys>:<seed>` through calls to
//!   owner instances. Each replies the ns the loop took.
//! - `owner:<i>:<owners>:<bucket>`: register `s28-owner-<i>` and hold the
//!   keys whose hash picks owner `i`; then answers `o:<key>` calls.
//! - `write:<bucket>:<count>:<keys>`: `<count>` overwrites of random keys,
//!   each value stamped with the time; replies the ns taken.
//! - `churn:<bucket>:<count>`: insert and delete a key `d...`, `<count>`
//!   times.
//! - `stats`: `<applied> <notices> <stale> <lag p50> <p99> <max> <last
//!   apply wall ns> <entries>`, lags in ns; then clears the lags.
//! - `holder`: register `s28-holder`, which never answers `hold`;
//!   `wait:<ms>`: call it, so this instance runs nothing for `<ms>`.
//! - `progress`: `<notices> <last apply wall ns>`.
//! - `check:<bucket>:<prefix>`: entries of the mirror that differ from a
//!   scan of the bucket, either way, and the first such key.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bulkhead_sdk::kv::Bucket;
use bulkhead_sdk::watch::{self, Change, WatchError};
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Info, Pid, Reply};

fn wall_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
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

fn key(prefix: &str, i: u64) -> String {
    format!("{prefix}{i:06}")
}

fn number(s: Option<&&str>) -> u64 {
    s.and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn owner_of(key: &str, owners: u64) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish() % owners.max(1)
}

/// FxHash (rustc's): a multiply and rotate per word, against std's
/// SipHash.
#[derive(Default)]
struct Fx(u64);

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..chunk.len()].copy_from_slice(chunk);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(w))
                .wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }
    fn finish(&self) -> u64 {
        // The multiply leaves the low bits weakest, and hashbrown picks a
        // bucket by them: rotate as rustc-hash 2 does.
        self.0.rotate_left(26)
    }
}

enum Map {
    Hash(HashMap<String, (Vec<u8>, u64)>),
    Fx(HashMap<String, (Vec<u8>, u64), BuildHasherDefault<Fx>>),
    Tree(BTreeMap<String, (Vec<u8>, u64)>),
}

impl Map {
    fn get(&self, k: &str) -> Option<&(Vec<u8>, u64)> {
        match self {
            Self::Hash(m) => m.get(k),
            Self::Fx(m) => m.get(k),
            Self::Tree(m) => m.get(k),
        }
    }
    fn insert(&mut self, k: String, v: (Vec<u8>, u64)) {
        match self {
            Self::Hash(m) => {
                m.insert(k, v);
            }
            Self::Fx(m) => {
                m.insert(k, v);
            }
            Self::Tree(m) => {
                m.insert(k, v);
            }
        }
    }
    fn remove(&mut self, k: &str) {
        match self {
            Self::Hash(m) => {
                m.remove(k);
            }
            Self::Fx(m) => {
                m.remove(k);
            }
            Self::Tree(m) => {
                m.remove(k);
            }
        }
    }
    fn len(&self) -> usize {
        match self {
            Self::Hash(m) => m.len(),
            Self::Fx(m) => m.len(),
            Self::Tree(m) => m.len(),
        }
    }
    fn clear(&mut self) {
        match self {
            Self::Hash(m) => m.clear(),
            Self::Fx(m) => m.clear(),
            Self::Tree(m) => m.clear(),
        }
    }
    fn iter(&self) -> Box<dyn Iterator<Item = (&String, &(Vec<u8>, u64))> + '_> {
        match self {
            Self::Hash(m) => Box::new(m.iter()),
            Self::Fx(m) => Box::new(m.iter()),
            Self::Tree(m) => Box::new(m.iter()),
        }
    }
}

/// The prototype: entries of `bucket` under `prefix`, kept by `watch`.
struct Mirror {
    bucket: String,
    watch: u64,
    map: Map,
    rescans: u64,
}

impl Mirror {
    fn scan_into(map: &mut Map, bucket: &str, prefix: &str) -> Result<u64, WatchError> {
        let scan = watch::scan(bucket).prefix(prefix);
        let mut cursor: Option<String> = None;
        let mut first = None;
        loop {
            let page = scan.page(cursor.as_deref())?;
            first.get_or_insert(page.revision);
            for e in page.entries {
                map.insert(e.key, (e.value, e.revision));
            }
            match page.next {
                Some(n) => cursor = Some(n),
                None => return Ok(first.unwrap_or(0)),
            }
        }
    }

    fn open(bucket: &str, prefix: &str, resume: bool, mut map: Map) -> Result<Self, WatchError> {
        if !resume {
            // Watching first means every change the scan may miss arrives
            // as a notice; nothing here can be too old.
            let started = watch::watch_prefix(bucket, prefix)?;
            Self::scan_into(&mut map, bucket, prefix)?;
            return Ok(Self {
                bucket: bucket.into(),
                watch: started.id,
                map,
                rescans: 0,
            });
        }
        let mut rescans = 0;
        loop {
            let from = Self::scan_into(&mut map, bucket, prefix)?;
            match watch::resume(bucket, prefix, true, from) {
                Ok(started) => {
                    return Ok(Self {
                        bucket: bucket.into(),
                        watch: started.id,
                        map,
                        rescans,
                    });
                }
                Err(WatchError::TooOld(_)) if rescans < 50 => {
                    rescans += 1;
                    map.clear();
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Apply a notice; the value now held, if the notice was this
    /// mirror's and newer than what it holds.
    fn apply(&mut self, change: &Change) -> Result<Applied, WatchError> {
        if change.watch != self.watch {
            return Ok(Applied::NotMine);
        }
        if self
            .map
            .get(&change.key)
            .is_some_and(|(_, r)| *r >= change.revision)
        {
            return Ok(Applied::Stale);
        }
        match watch::get(&self.bucket, &change.key)? {
            Some(e) => {
                let stamp = e
                    .value
                    .get(..8)
                    .map(|b| u64::from_le_bytes(b.try_into().unwrap()));
                self.map.insert(change.key.clone(), (e.value, e.revision));
                Ok(Applied::Set(stamp))
            }
            None => {
                self.map.remove(&change.key);
                Ok(Applied::Deleted)
            }
        }
    }
}

enum Applied {
    NotMine,
    Stale,
    Set(Option<u64>),
    Deleted,
}

#[derive(Default)]
struct S28 {
    mirror: Option<Mirror>,
    owned: HashMap<String, Vec<u8>>,
    owners: Vec<Pid>,
    rng: u64,
    notices: u64,
    applied: u64,
    stale: u64,
    lags: Vec<u64>,
    last_apply: u64,
}

fn fill(f: &[&str]) -> Result<String, String> {
    let bucket = Bucket::open(f[0]).map_err(|e| e.to_string())?;
    let (n, size) = (number(f.get(1)), number(f.get(2)).max(8));
    let mut i = 0;
    while i < n {
        let now = wall_ns().to_le_bytes();
        let batch: Vec<(String, Vec<u8>)> = (i..n.min(i + 1000))
            .map(|j| {
                let mut v = vec![7u8; size as usize];
                v[..8].copy_from_slice(&now);
                (key("k", j), v)
            })
            .collect();
        i += batch.len() as u64;
        bucket.set_many(&batch).map_err(|e| e.to_string())?;
    }
    Ok("ok".into())
}

fn fill_chain(f: &[&str]) -> Result<String, String> {
    let bucket = Bucket::open(f[0]).map_err(|e| e.to_string())?;
    let n = number(f.get(1)).max(1);
    let mut i = 0;
    while i < n {
        let batch: Vec<(String, Vec<u8>)> = (i..n.min(i + 1000))
            .map(|j| (key("c", j), key("c", (7919 * j + 1) % n).into_bytes()))
            .collect();
        i += batch.len() as u64;
        bucket.set_many(&batch).map_err(|e| e.to_string())?;
    }
    Ok("ok".into())
}

fn linear_memory() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        core::arch::wasm32::memory_size(0) as u64 * 65536
    }
    #[cfg(not(target_arch = "wasm32"))]
    0
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

impl S28 {
    fn command(&mut self, ctx: &mut Ctx, text: &str) -> Result<String, String> {
        let (verb, rest) = text.split_once(':').unwrap_or((text, ""));
        let f: Vec<&str> = rest.split(':').collect();
        let e = |e: WatchError| e.to_string();
        match verb {
            "fill" => fill(&f),
            "fillchain" => fill_chain(&f),
            "mirror" => {
                let map = match f.get(3) {
                    Some(&"tree") => Map::Tree(BTreeMap::new()),
                    Some(&"fx") => Map::Fx(HashMap::default()),
                    _ => Map::Hash(HashMap::new()),
                };
                let started = Instant::now();
                let m = Mirror::open(f[0], f[1], f.get(2) == Some(&"resume"), map).map_err(e)?;
                let ns = started.elapsed().as_nanos();
                let reply = format!("{} {ns} {} {}", m.map.len(), m.rescans, linear_memory());
                self.mirror = Some(m);
                Ok(reply)
            }
            "mget" | "mchain" => {
                let m = self.mirror.as_ref().ok_or("no mirror")?;
                let (n, keys) = (number(f.first()), number(f.get(1)));
                let mut rng = Lcg(number(f.get(2)));
                let mut out = Vec::with_capacity(64);
                if verb == "mget" {
                    let picked: Vec<String> = (0..n).map(|_| key("k", rng.next(keys))).collect();
                    let started = Instant::now();
                    for k in &picked {
                        out.clear();
                        if let Some((v, _)) = m.map.get(k) {
                            out.extend_from_slice(v);
                        }
                        std::hint::black_box(&out);
                    }
                    return Ok(started.elapsed().as_nanos().to_string());
                }
                let mut at = key("c", number(f.get(2)) % keys.max(1));
                let started = Instant::now();
                for _ in 0..n {
                    let next = m
                        .map
                        .get(&at)
                        .map(|(v, _)| v.as_slice())
                        .unwrap_or_default();
                    at = String::from_utf8(next.to_vec()).map_err(|e| e.to_string())?;
                }
                std::hint::black_box(at);
                Ok(started.elapsed().as_nanos().to_string())
            }
            "bget" | "bchain" => {
                let bucket = Bucket::open(f[0]).map_err(|e| e.to_string())?;
                let (n, keys) = (number(f.get(1)), number(f.get(2)));
                let mut rng = Lcg(number(f.get(3)));
                if verb == "bget" {
                    let picked: Vec<String> = (0..n).map(|_| key("k", rng.next(keys))).collect();
                    let started = Instant::now();
                    for k in &picked {
                        std::hint::black_box(bucket.get(k).map_err(|e| e.to_string())?);
                    }
                    return Ok(started.elapsed().as_nanos().to_string());
                }
                let mut at = key("c", number(f.get(3)) % keys.max(1));
                let started = Instant::now();
                for _ in 0..n {
                    let next = bucket
                        .get(&at)
                        .map_err(|e| e.to_string())?
                        .unwrap_or_default();
                    at = String::from_utf8(next).map_err(|e| e.to_string())?;
                }
                std::hint::black_box(at);
                Ok(started.elapsed().as_nanos().to_string())
            }
            "owner" => {
                let (i, owners) = (number(f.first()), number(f.get(1)));
                ctx.register(&format!("s28-owner-{i}"))
                    .map_err(|e| format!("{e:?}"))?;
                for entry in watch::scan(f[2]).prefix("k").entries() {
                    let entry = entry.map_err(e)?;
                    if owner_of(&entry.key, owners) == i {
                        self.owned.insert(entry.key, entry.value);
                    }
                }
                Ok(self.owned.len().to_string())
            }
            "o" => Ok(String::new()), // handled in handle_call as bytes
            "oget" => {
                let (owners, n, keys) = (number(f.first()), number(f.get(1)), number(f.get(2)));
                if self.owners.len() as u64 != owners {
                    self.owners = (0..owners)
                        .map(|i| ctx.whereis(&format!("s28-owner-{i}")).ok_or("no owner"))
                        .collect::<Result<_, _>>()?;
                }
                let mut rng = Lcg(number(f.get(3)));
                let picked: Vec<String> = (0..n).map(|_| key("k", rng.next(keys))).collect();
                let started = Instant::now();
                for k in &picked {
                    let to = self.owners[owner_of(k, owners) as usize];
                    let reply = ctx
                        .call(
                            to,
                            Envelope::text(format!("o:{k}")),
                            std::time::Duration::from_secs(10),
                        )
                        .map_err(|e| format!("{e:?}"))?;
                    std::hint::black_box(reply.body);
                }
                Ok(started.elapsed().as_nanos().to_string())
            }
            "write" => {
                let bucket = Bucket::open(f[0]).map_err(|e| e.to_string())?;
                let (count, keys) = (number(f.get(1)), number(f.get(2)));
                let mut rng = Lcg(self.rng);
                let started = Instant::now();
                let mut v = [7u8; 16];
                for _ in 0..count {
                    let k = key("k", rng.next(keys));
                    v[..8].copy_from_slice(&wall_ns().to_le_bytes());
                    bucket.set(&k, &v).map_err(|e| e.to_string())?;
                }
                self.rng = rng.0;
                Ok(started.elapsed().as_nanos().to_string())
            }
            "churn" => {
                let bucket = Bucket::open(f[0]).map_err(|e| e.to_string())?;
                for _ in 0..number(f.get(1)) {
                    self.rng += 1;
                    let k = key("d", self.rng);
                    bucket.set(&k, b"x").map_err(|e| e.to_string())?;
                    bucket.delete(&k).map_err(|e| e.to_string())?;
                }
                Ok("ok".into())
            }
            "stats" => {
                let mut lags = std::mem::take(&mut self.lags);
                lags.sort_unstable();
                Ok(format!(
                    "{} {} {} {} {} {} {} {}",
                    self.applied,
                    self.notices,
                    self.stale,
                    percentile(&lags, 0.5),
                    percentile(&lags, 0.99),
                    lags.last().copied().unwrap_or(0),
                    self.last_apply,
                    self.mirror.as_ref().map_or(0, |m| m.map.len()),
                ))
            }
            "check" => {
                let m = self.mirror.as_ref().ok_or("no mirror")?;
                let mut bucket = HashMap::new();
                for entry in watch::scan(f[0]).prefix(f[1]).entries() {
                    let entry = entry.map_err(e)?;
                    bucket.insert(entry.key, (entry.value, entry.revision));
                }
                let mut differ = 0u64;
                let mut first = String::new();
                for (k, (v, r)) in m.map.iter() {
                    let b = bucket.get(k);
                    if b.map(|(bv, _)| bv) != Some(v) {
                        differ += 1;
                        if first.is_empty() {
                            first = format!(" {k} mirror@{r} bucket@{:?}", b.map(|(_, br)| br));
                        }
                    }
                }
                for k in bucket.keys().filter(|k| m.map.get(k).is_none()) {
                    differ += 1;
                    if first.is_empty() {
                        first = format!(" {k} missing from the mirror");
                    }
                }
                Ok(format!("{differ}{first}"))
            }
            "wait" => {
                // Held in a call that is never answered, so notices queue
                // and none runs: what a writer pays without readers
                // competing for workers.
                let holder = ctx.whereis("s28-holder").ok_or("no holder")?;
                let ms = number(f.first());
                let _ = ctx.call(
                    holder,
                    Envelope::text("hold"),
                    std::time::Duration::from_millis(ms),
                );
                Ok("ok".into())
            }
            "holder" => {
                ctx.register("s28-holder").map_err(|e| format!("{e:?}"))?;
                Ok("ok".into())
            }
            "progress" => Ok(format!("{} {}", self.notices, self.last_apply)),
            "mem" => Ok(linear_memory().to_string()),
            other => Err(format!("unknown command {other}")),
        }
    }
}

impl App for S28 {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self {
            rng: 1,
            ..Self::default()
        })
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        if text == "hold" {
            return Reply::Later;
        }
        if let Some(k) = text.strip_prefix("o:") {
            let v = self.owned.get(k).cloned().unwrap_or_default();
            return Reply::Now(Envelope::new("application/octet-stream", v));
        }
        let reply = self
            .command(ctx, text)
            .unwrap_or_else(|e| format!("err:{e}"));
        Reply::Now(Envelope::text(reply))
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
        let Some(change) = Change::from_info(&info) else {
            return;
        };
        let Some(m) = self.mirror.as_mut() else {
            return;
        };
        self.notices += 1;
        match m.apply(&change) {
            Ok(Applied::Set(stamp)) => {
                self.applied += 1;
                let now = wall_ns();
                self.last_apply = now;
                if let Some(t) = stamp
                    && self.lags.len() < 4_000_000
                {
                    self.lags.push(now.saturating_sub(t));
                }
            }
            Ok(Applied::Deleted) => {
                self.applied += 1;
                self.last_apply = wall_ns();
            }
            Ok(Applied::Stale) => self.stale += 1,
            Ok(Applied::NotMine) | Err(_) => {}
        }
    }
}

bulkhead_sdk::app!(S28);
