//! Term storage behind `wasi:keyvalue`.
//!
//! Buckets are host-owned, so their data outlives instance crashes. Each is
//! namespaced by app: two apps opening a bucket of the same name see
//! different data. `memory` buckets live in the runtime; `disk` buckets live
//! in one redb file under the node's data directory and survive restarts.
//!
//! Every operation runs as one transaction against a [`Txn`], so both
//! backends share the quota, counter and paging rules. Disk writes run on
//! tokio's blocking pool, keeping fsyncs off the workers that run guests;
//! disk reads run on the calling worker, usually from redb's page cache.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use paddock_bundle::manifest::Durability;
use redb::{ReadableDatabase, ReadableTable, TableDefinition};

/// Keys per `list-keys` page.
pub(crate) const PAGE: usize = 256;

/// A bucket an instance opened: the resource behind `wasi:keyvalue`'s
/// `bucket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketHandle {
    pub(crate) app: String,
    pub(crate) name: String,
    pub(crate) durability: Durability,
    pub(crate) quota: Option<u64>,
}

/// One storage operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    Get(String),
    Set(String, Vec<u8>),
    Delete(String),
    Exists(String),
    Keys(Option<u64>),
    Increment(String, u64),
    GetMany(Vec<String>),
    SetMany(Vec<(String, Vec<u8>)>),
    DeleteMany(Vec<String>),
}

impl Op {
    fn read_only(&self) -> bool {
        matches!(
            self,
            Self::Get(_) | Self::Exists(_) | Self::Keys(_) | Self::GetMany(_)
        )
    }
}

/// What an operation returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Done,
    Value(Option<Vec<u8>>),
    Exists(bool),
    Keys(Vec<String>, Option<u64>),
    Count(u64),
    Values(Vec<Option<(String, Vec<u8>)>>),
}

/// The view of one bucket an operation runs against.
pub(crate) trait Txn {
    fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, String>;
    fn put(&mut self, key: &str, value: &[u8]) -> Result<(), String>;
    fn remove(&mut self, key: &str) -> Result<(), String>;
    /// Keys in order, from `offset`, at most `limit`.
    fn keys(&mut self, offset: usize, limit: usize) -> Result<Vec<String>, String>;
    /// Bytes stored: the sum of key and value lengths.
    fn bytes(&mut self) -> Result<u64, String>;
}

fn entry_size(key: &str, value: &[u8]) -> u64 {
    u64::try_from(key.len() + value.len()).unwrap_or(u64::MAX)
}

/// Run `op` against `txn`. On success, returns the outcome and the bucket's
/// new size in bytes.
pub(crate) fn apply(
    txn: &mut dyn Txn,
    op: Op,
    quota: Option<u64>,
) -> Result<(Outcome, u64), String> {
    let mut bytes = txn.bytes()?;
    let outcome = match op {
        Op::Get(key) => Outcome::Value(txn.get(&key)?),
        Op::Exists(key) => Outcome::Exists(txn.get(&key)?.is_some()),
        Op::Keys(cursor) => {
            let offset = usize::try_from(cursor.unwrap_or(0)).unwrap_or(usize::MAX);
            let mut keys = txn.keys(offset, PAGE + 1)?;
            let more = keys.len() > PAGE;
            keys.truncate(PAGE);
            let next = more.then(|| u64::try_from(offset + PAGE).unwrap_or(u64::MAX));
            Outcome::Keys(keys, next)
        }
        Op::GetMany(keys) => {
            let mut found = Vec::with_capacity(keys.len());
            for key in keys {
                found.push(txn.get(&key)?.map(|v| (key, v)));
            }
            Outcome::Values(found)
        }
        Op::Set(key, value) => {
            bytes = set(txn, bytes, &key, &value, quota)?;
            Outcome::Done
        }
        Op::SetMany(pairs) => {
            // Check the whole batch first, so it fails or succeeds as one.
            let mut after = bytes;
            for (key, value) in &pairs {
                let old = txn.get(key)?.map_or(0, |v| entry_size(key, &v));
                after = after
                    .saturating_sub(old)
                    .saturating_add(entry_size(key, value));
            }
            check_quota(after, quota)?;
            for (key, value) in &pairs {
                bytes = set(txn, bytes, key, value, None)?;
            }
            Outcome::Done
        }
        Op::Delete(key) => {
            bytes = delete(txn, bytes, &key)?;
            Outcome::Done
        }
        Op::DeleteMany(keys) => {
            for key in &keys {
                bytes = delete(txn, bytes, key)?;
            }
            Outcome::Done
        }
        Op::Increment(key, delta) => {
            // Counters are decimal strings, as in wasmtime's keyvalue host.
            let current = match txn.get(&key)? {
                None => 0,
                Some(v) => std::str::from_utf8(&v)
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| format!("value of '{key}' is not a decimal number"))?,
            };
            let next = current
                .checked_add(delta)
                .ok_or_else(|| format!("incrementing '{key}' overflows"))?;
            bytes = set(txn, bytes, &key, next.to_string().as_bytes(), quota)?;
            Outcome::Count(next)
        }
    };
    Ok((outcome, bytes))
}

fn check_quota(bytes: u64, quota: Option<u64>) -> Result<(), String> {
    match quota {
        Some(q) if bytes > q => Err(format!("bucket quota of {q} bytes exceeded")),
        _ => Ok(()),
    }
}

fn set(
    txn: &mut dyn Txn,
    bytes: u64,
    key: &str,
    value: &[u8],
    quota: Option<u64>,
) -> Result<u64, String> {
    let old = txn.get(key)?.map_or(0, |v| entry_size(key, &v));
    let after = bytes
        .saturating_sub(old)
        .saturating_add(entry_size(key, value));
    check_quota(after, quota)?;
    txn.put(key, value)?;
    Ok(after)
}

fn delete(txn: &mut dyn Txn, bytes: u64, key: &str) -> Result<u64, String> {
    let old = txn.get(key)?.map_or(0, |v| entry_size(key, &v));
    txn.remove(key)?;
    Ok(bytes.saturating_sub(old))
}

/// A memory bucket.
#[derive(Debug, Default)]
pub(crate) struct MemBucket {
    entries: BTreeMap<String, Vec<u8>>,
    bytes: u64,
}

impl Txn for MemBucket {
    fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.entries.get(key).cloned())
    }

    fn put(&mut self, key: &str, value: &[u8]) -> Result<(), String> {
        self.entries.insert(key.to_string(), value.to_vec());
        Ok(())
    }

    fn remove(&mut self, key: &str) -> Result<(), String> {
        self.entries.remove(key);
        Ok(())
    }

    fn keys(&mut self, offset: usize, limit: usize) -> Result<Vec<String>, String> {
        Ok(self
            .entries
            .keys()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect())
    }

    fn bytes(&mut self) -> Result<u64, String> {
        Ok(self.bytes)
    }
}

type Key = (String, String);

/// Every bucket on the node.
#[derive(Debug)]
pub(crate) struct KvStore {
    memory: Mutex<HashMap<Key, MemBucket>>,
    disk: Option<Arc<Disk>>,
}

#[derive(Debug)]
struct Disk {
    db: redb::Database,
    /// Bucket sizes, computed on first use and kept after each commit.
    sizes: Mutex<HashMap<Key, u64>>,
}

impl KvStore {
    /// Open the store; disk buckets need `data_dir`.
    pub(crate) fn open(data_dir: Option<&Path>) -> Result<Self, String> {
        let disk = match data_dir {
            None => None,
            Some(dir) => {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                let path = dir.join("keyvalue.redb");
                let db = redb::Database::create(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                Some(Arc::new(Disk {
                    db,
                    sizes: Mutex::default(),
                }))
            }
        };
        Ok(Self {
            memory: Mutex::default(),
            disk,
        })
    }

    /// Run one operation against a bucket.
    pub(crate) async fn run(&self, bucket: &BucketHandle, op: Op) -> Result<Outcome, String> {
        let key = (bucket.app.clone(), bucket.name.clone());
        match bucket.durability {
            Durability::Memory => {
                let mut memory = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
                let b = memory.entry(key).or_default();
                let (outcome, bytes) = apply(b, op, bucket.quota)?;
                b.bytes = bytes;
                Ok(outcome)
            }
            Durability::Disk => {
                let disk = self
                    .disk
                    .clone()
                    .ok_or("disk storage is not configured on this node")?;
                let quota = bucket.quota;
                // Reads run here: redb usually serves them from its page
                // cache in a microsecond or two, and a hop to the blocking
                // pool costs about 20 µs (spike S14). A cache miss blocks
                // this worker for one read from disk. Writes fsync, so they
                // stay off the worker.
                if op.read_only() {
                    return disk.run(&key, op, quota);
                }
                tokio::task::spawn_blocking(move || disk.run(&key, op, quota))
                    .await
                    .map_err(|e| format!("storage task failed: {e}"))?
            }
            Durability::Replicated => {
                Err("replicated buckets are not supported until Phase 3".into())
            }
        }
    }
}

impl Disk {
    fn run(&self, key: &Key, op: Op, quota: Option<u64>) -> Result<Outcome, String> {
        let name = format!("{}/{}", key.0, key.1);
        let table: TableDefinition<&str, &[u8]> = TableDefinition::new(&name);
        let cached = self
            .sizes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .copied();
        let e = |err: &dyn std::fmt::Display| err.to_string();
        if op.read_only() {
            let txn = self.db.begin_read().map_err(|err| e(&err))?;
            let mut view = match txn.open_table(table) {
                Ok(t) => DiskRead {
                    table: Some(t),
                    cached,
                },
                Err(redb::TableError::TableDoesNotExist(_)) => DiskRead {
                    table: None,
                    cached,
                },
                Err(err) => return Err(e(&err)),
            };
            let (outcome, bytes) = apply(&mut view, op, quota)?;
            self.remember(key, bytes);
            return Ok(outcome);
        }
        let txn = self.db.begin_write().map_err(|err| e(&err))?;
        let (outcome, bytes) = {
            let t = txn.open_table(table).map_err(|err| e(&err))?;
            let mut view = DiskWrite { table: t, cached };
            apply(&mut view, op, quota)?
        };
        txn.commit().map_err(|err| e(&err))?;
        self.remember(key, bytes);
        Ok(outcome)
    }

    fn remember(&self, key: &Key, bytes: u64) {
        self.sizes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), bytes);
    }
}

fn scan_bytes<T: ReadableTable<&'static str, &'static [u8]>>(table: &T) -> Result<u64, String> {
    let mut total = 0u64;
    for entry in table.iter().map_err(|e| e.to_string())? {
        let (k, v) = entry.map_err(|e| e.to_string())?;
        total = total.saturating_add(entry_size(k.value(), v.value()));
    }
    Ok(total)
}

fn read_keys<T: ReadableTable<&'static str, &'static [u8]>>(
    table: &T,
    offset: usize,
    limit: usize,
) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    for entry in table
        .iter()
        .map_err(|e| e.to_string())?
        .skip(offset)
        .take(limit)
    {
        let (k, _) = entry.map_err(|e| e.to_string())?;
        keys.push(k.value().to_string());
    }
    Ok(keys)
}

struct DiskRead {
    table: Option<redb::ReadOnlyTable<&'static str, &'static [u8]>>,
    cached: Option<u64>,
}

impl Txn for DiskRead {
    fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(t) = &self.table else {
            return Ok(None);
        };
        Ok(t.get(key)
            .map_err(|e| e.to_string())?
            .map(|v| v.value().to_vec()))
    }

    fn put(&mut self, _key: &str, _value: &[u8]) -> Result<(), String> {
        Err("write in a read transaction".into())
    }

    fn remove(&mut self, _key: &str) -> Result<(), String> {
        Err("write in a read transaction".into())
    }

    fn keys(&mut self, offset: usize, limit: usize) -> Result<Vec<String>, String> {
        match &self.table {
            Some(t) => read_keys(t, offset, limit),
            None => Ok(Vec::new()),
        }
    }

    fn bytes(&mut self) -> Result<u64, String> {
        if let Some(b) = self.cached {
            return Ok(b);
        }
        let b = match &self.table {
            Some(t) => scan_bytes(t)?,
            None => 0,
        };
        self.cached = Some(b);
        Ok(b)
    }
}

struct DiskWrite<'txn> {
    table: redb::Table<'txn, &'static str, &'static [u8]>,
    cached: Option<u64>,
}

impl Txn for DiskWrite<'_> {
    fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .table
            .get(key)
            .map_err(|e| e.to_string())?
            .map(|v| v.value().to_vec()))
    }

    fn put(&mut self, key: &str, value: &[u8]) -> Result<(), String> {
        self.table.insert(key, value).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn remove(&mut self, key: &str) -> Result<(), String> {
        self.table.remove(key).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn keys(&mut self, offset: usize, limit: usize) -> Result<Vec<String>, String> {
        read_keys(&self.table, offset, limit)
    }

    fn bytes(&mut self) -> Result<u64, String> {
        if let Some(b) = self.cached {
            return Ok(b);
        }
        let b = scan_bytes(&self.table)?;
        self.cached = Some(b);
        Ok(b)
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod storage_tests;
