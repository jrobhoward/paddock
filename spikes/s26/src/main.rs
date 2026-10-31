//! Spike S26: how a bucket's map scales with readers, outside Paddock.
//!
//! One map of 100,000 keys (`k000000` ...) with 16 B values, read by 1 to
//! N threads (a read copies the value out, as the keyvalue host does),
//! with no writer or with one thread overwriting random keys as fast as it
//! can. Reads per second across the readers, writes per second, and how
//! many reads found no value (an overwrite must never hide a key).
//!
//! ```sh
//! cargo run --release -p s26-bench -- [max readers] [seconds per row]
//! ```

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, RwLock};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossbeam_skiplist::SkipMap;
use crossbeam_utils::sync::ShardedLock;
use scc::TreeIndex;

const KEYS: usize = 100_000;

/// A bucket's map as the keyvalue host would use it.
trait Store: Send + Sync {
    fn get(&self, key: &str) -> Option<Vec<u8>>;
    fn put(&self, key: &str, value: &[u8]);
    fn remove(&self, key: &str);
}

type Map = BTreeMap<String, Vec<u8>>;

struct MutexMap(Mutex<Map>);
struct RwMap(RwLock<Map>);
struct ShardedMap(ShardedLock<Map>);
struct Skip(SkipMap<String, Vec<u8>>);
/// Copy-on-write: a writer clones the whole map.
struct Cow(ArcSwap<Map>);
/// A skip list whose entries swap their values in place: an overwrite
/// never removes the key.
struct SkipSwap(SkipMap<String, ArcSwap<Vec<u8>>>);
/// A sharded lock over a map whose entries swap their values in place: an
/// overwrite takes only a read lock; inserts and deletes take the write
/// lock.
struct ShardedSwap(ShardedLock<BTreeMap<String, ArcSwap<Vec<u8>>>>);

impl Store for MutexMap {
    fn remove(&self, key: &str) {
        self.0.lock().unwrap().remove(key);
    }
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned()
    }
    fn put(&self, key: &str, value: &[u8]) {
        self.0.lock().unwrap().insert(key.to_string(), value.to_vec());
    }
}

impl Store for RwMap {
    fn remove(&self, key: &str) {
        self.0.write().unwrap().remove(key);
    }
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.read().unwrap().get(key).cloned()
    }
    fn put(&self, key: &str, value: &[u8]) {
        self.0.write().unwrap().insert(key.to_string(), value.to_vec());
    }
}

impl Store for ShardedMap {
    fn remove(&self, key: &str) {
        self.0.write().unwrap().remove(key);
    }
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.read().unwrap().get(key).cloned()
    }
    fn put(&self, key: &str, value: &[u8]) {
        self.0.write().unwrap().insert(key.to_string(), value.to_vec());
    }
}

impl Store for Skip {
    fn remove(&self, key: &str) {
        self.0.remove(key);
    }
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.get(key).map(|e| e.value().clone())
    }
    fn put(&self, key: &str, value: &[u8]) {
        self.0.insert(key.to_string(), value.to_vec());
    }
}

impl Store for Cow {
    fn remove(&self, key: &str) {
        self.0.rcu(|m| {
            let mut m = Map::clone(m);
            m.remove(key);
            m
        });
    }
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.load().get(key).cloned()
    }
    fn put(&self, key: &str, value: &[u8]) {
        self.0.rcu(|m| {
            let mut m = Map::clone(m);
            m.insert(key.to_string(), value.to_vec());
            m
        });
    }
}

impl Store for SkipSwap {
    fn remove(&self, key: &str) {
        self.0.remove(key);
    }
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.get(key).map(|e| Vec::clone(&e.value().load()))
    }
    fn put(&self, key: &str, value: &[u8]) {
        match self.0.get(key) {
            Some(e) => e.value().store(Arc::new(value.to_vec())),
            None => {
                self.0
                    .insert(key.to_string(), ArcSwap::from_pointee(value.to_vec()));
            }
        }
    }
}

impl Store for ShardedSwap {
    fn remove(&self, key: &str) {
        self.0.write().unwrap().remove(key);
    }
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0
            .read()
            .unwrap()
            .get(key)
            .map(|v| Vec::clone(&v.load()))
    }
    fn put(&self, key: &str, value: &[u8]) {
        if let Some(v) = self.0.read().unwrap().get(key) {
            v.store(Arc::new(value.to_vec()));
            return;
        }
        self.0
            .write()
            .unwrap()
            .insert(key.to_string(), ArcSwap::from_pointee(value.to_vec()));
    }
}

/// A concurrent B+-tree with lock-free reads (scc), whose entries swap
/// their values in place.
struct TreeSwap(TreeIndex<String, Arc<ArcSwap<Vec<u8>>>>);

impl Store for TreeSwap {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.peek_with(key, |_, v| Vec::clone(&v.load()))
    }
    fn put(&self, key: &str, value: &[u8]) {
        let found = self
            .0
            .peek_with(key, |_, v| v.store(Arc::new(value.to_vec())));
        if found.is_none() {
            let _ = self.0.insert_sync(
                key.to_string(),
                Arc::new(ArcSwap::from_pointee(value.to_vec())),
            );
        }
    }
    fn remove(&self, key: &str) {
        self.0.remove_sync(key);
    }
}

fn keys() -> Vec<String> {
    (0..KEYS).map(|i| format!("k{i:06}")).collect()
}

fn filled() -> Map {
    keys().into_iter().map(|k| (k, vec![7u8; 16])).collect()
}

/// The order a thread reads keys in: the kvbench generator's sequence.
fn sequence(seed: u64, n: usize) -> Vec<usize> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((x >> 33) % KEYS as u64) as usize
        })
        .collect()
}

struct Row {
    reads: f64,
    writes: f64,
    misses: u64,
}

/// What the writer does: nothing, overwrite existing keys, or insert a new
/// key and delete it again (two writes).
#[derive(Clone, Copy, PartialEq)]
enum Writer {
    None,
    Overwrite,
    InsertDelete,
}

fn run(store: Arc<dyn Store>, readers: usize, writer: Writer, secs: f64) -> Row {
    let keys = Arc::new(keys());
    let stop = Arc::new(AtomicBool::new(false));
    let total = Arc::new(AtomicU64::new(0));
    let misses = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let start = Arc::new(Barrier::new(readers + usize::from(writer != Writer::None) + 1));
    let mut threads = Vec::new();
    for r in 0..readers {
        let (store, keys, stop, total, misses, start) = (
            store.clone(),
            keys.clone(),
            stop.clone(),
            total.clone(),
            misses.clone(),
            start.clone(),
        );
        threads.push(std::thread::spawn(move || {
            let seq = sequence(r as u64, 1 << 20);
            start.wait();
            let (mut n, mut missed) = (0u64, 0u64);
            'outer: loop {
                for &i in &seq {
                    if store.get(&keys[i]).is_none() {
                        missed += 1;
                    }
                    n += 1;
                    if n % 1024 == 0 && stop.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                }
            }
            total.fetch_add(n, Ordering::Relaxed);
            misses.fetch_add(missed, Ordering::Relaxed);
        }));
    }
    if writer != Writer::None {
        let (store, keys, stop, writes, start) = (
            store.clone(),
            keys.clone(),
            stop.clone(),
            writes.clone(),
            start.clone(),
        );
        threads.push(std::thread::spawn(move || {
            let seq = sequence(99, 1 << 16);
            let value = [9u8; 16];
            start.wait();
            let mut n = 0u64;
            let fresh: Vec<String> = (0..1 << 16).map(|i| format!("n{i:06}")).collect();
            'outer: loop {
                for (j, &i) in seq.iter().enumerate() {
                    if writer == Writer::Overwrite {
                        store.put(&keys[i], &value);
                        n += 1;
                    } else {
                        store.put(&fresh[j], &value);
                        store.remove(&fresh[j]);
                        n += 2;
                    }
                    if stop.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                }
            }
            writes.fetch_add(n, Ordering::Relaxed);
        }));
    }
    start.wait();
    let began = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(secs));
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().unwrap();
    }
    let elapsed = began.elapsed().as_secs_f64();
    Row {
        reads: total.load(Ordering::Relaxed) as f64 / elapsed,
        writes: writes.load(Ordering::Relaxed) as f64 / elapsed,
        misses: misses.load(Ordering::Relaxed),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let max: usize = args.first().map_or(16, |a| a.parse().unwrap());
    let secs: f64 = args.get(1).map_or(2.0, |a| a.parse().unwrap());
    let counts: Vec<usize> = [1, 2, 4, 8, 16, 32]
        .into_iter()
        .filter(|&n| n <= max)
        .collect();
    println!(
        "{} cores; {KEYS} keys, 16 B values; {secs} s per row",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    println!(
        "{:<12} {:>7} {:>7} {:>14} {:>12} {:>7}",
        "map", "readers", "writer", "reads/s", "writes/s", "misses"
    );
    let stores: Vec<(&str, Box<dyn Fn() -> Arc<dyn Store>>)> = vec![
        ("mutex", Box::new(|| Arc::new(MutexMap(Mutex::new(filled()))))),
        ("rwlock", Box::new(|| Arc::new(RwMap(RwLock::new(filled()))))),
        ("sharded", Box::new(|| Arc::new(ShardedMap(ShardedLock::new(filled()))))),
        ("skiplist", Box::new(|| {
            let m = SkipMap::new();
            for (k, v) in filled() {
                m.insert(k, v);
            }
            Arc::new(Skip(m))
        })),
        ("cow", Box::new(|| Arc::new(Cow(ArcSwap::from_pointee(filled()))))),
        ("skipswap", Box::new(|| {
            let m = SkipMap::new();
            for (k, v) in filled() {
                m.insert(k, ArcSwap::from_pointee(v));
            }
            Arc::new(SkipSwap(m))
        })),
        ("treeswap", Box::new(|| {
            let m = TreeIndex::new();
            for (k, v) in filled() {
                let _ = m.insert_sync(k, Arc::new(ArcSwap::from_pointee(v)));
            }
            Arc::new(TreeSwap(m))
        })),
        ("shardswap", Box::new(|| {
            let m = filled()
                .into_iter()
                .map(|(k, v)| (k, ArcSwap::from_pointee(v)))
                .collect();
            Arc::new(ShardedSwap(ShardedLock::new(m)))
        })),
    ];
    let only = std::env::var("S26_ONLY").ok();
    for (name, make) in &stores {
        if only.as_deref().is_some_and(|o| !o.split(',').any(|x| x == *name)) {
            continue;
        }
        for (writer, label) in [
            (Writer::None, "none"),
            (Writer::Overwrite, "set"),
            (Writer::InsertDelete, "ins+del"),
        ] {
            if writer == Writer::InsertDelete && *name == "cow" {
                continue;
            }
            for &n in &counts {
                let row = run(make(), n, writer, secs);
                println!(
                    "{name:<12} {n:>7} {:>7} {:>14.0} {:>12.0} {:>7}",
                    label,
                    row.reads,
                    row.writes,
                    row.misses
                );
            }
        }
    }
}
