//! Term storage behind `wasi:keyvalue`.
//!
//! Buckets are host-owned, so their data outlives instance crashes. An app
//! bucket is namespaced by app: two apps opening a bucket of the same name
//! see different data. A shared bucket (node configuration's
//! `[buckets.<name>]`) lives in one namespace, [`SHARED`], for every app
//! granted it. `memory` buckets live in the runtime; `disk` buckets live in
//! one redb file under the node's data directory and survive restarts.
//!
//! Every operation runs as one transaction against a [`Txn`], so both
//! backends share the quota, counter, paging and revision rules. Each
//! transaction that changes a bucket raises the bucket's revision by one
//! and stamps every key it writes with the new revision (etcd's model), for
//! watches and compare-and-swap (`paddock:watch`). A key deleted loses its
//! revision; keys written before revisions existed read as revision 0. Each
//! bucket also keeps the revision of its latest delete, so a watch resuming
//! from an earlier revision knows it cannot replay what it missed. Disk writes run on
//! tokio's blocking pool, keeping fsyncs off the workers that run guests;
//! disk reads run on the calling worker, usually from redb's page cache.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use paddock_bundle::manifest::Durability;
use redb::{ReadableDatabase, ReadableTable, TableDefinition, TableHandle};

use tokio::sync::watch;

use super::watches::Watches;
use crate::config::SharedBucket;
use crate::replication::{Handle, Refusal, ReplicationStatus, SPACE};

/// Keys per `list-keys` page.
pub(crate) const PAGE: usize = 256;

/// The namespace of shared buckets. App names cannot start with `@`.
pub(crate) const SHARED: &str = "@shared";

/// A bucket an instance opened: the resource behind `wasi:keyvalue`'s
/// `bucket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketHandle {
    /// The app's name for an app bucket, [`SHARED`] for a shared one.
    pub(crate) space: String,
    pub(crate) name: String,
    pub(crate) durability: Durability,
    pub(crate) quota: Option<u64>,
    /// `false` for a shared bucket granted read-only.
    pub(crate) writable: bool,
}

/// One storage operation. Serializable: a replicated bucket's log carries
/// its writes ([`crate::replication`]), and a standby forwards them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Op {
    Get(String),
    Set(
        String,
        #[serde(with = "crate::transport::wire::bytes")] Vec<u8>,
    ),
    Delete(String),
    Exists(String),
    Keys(Option<u64>),
    Increment(String, u64),
    GetMany(Vec<String>),
    SetMany(#[serde(with = "crate::transport::wire::blobs::pairs")] Vec<(String, Vec<u8>)>),
    DeleteMany(Vec<String>),
    /// The bucket's revision and the key's.
    Revision(String),
    /// The value and its revision.
    Entry(String),
    /// Keys matching `key` (or starting with it, for `prefix`) changed
    /// after revision `after`, for a watch resuming.
    Since {
        key: String,
        prefix: bool,
        after: u64,
    },
    /// Set the value if the key's revision is `expected` (`None`: absent).
    Swap {
        key: String,
        expected: Option<u64>,
        #[serde(with = "crate::transport::wire::bytes")]
        value: Vec<u8>,
    },
    /// Delete the key if its revision is `expected`.
    SwapDelete {
        key: String,
        expected: u64,
    },
}

impl Op {
    pub(crate) fn read_only(&self) -> bool {
        matches!(
            self,
            Self::Get(_)
                | Self::Exists(_)
                | Self::Keys(_)
                | Self::GetMany(_)
                | Self::Revision(_)
                | Self::Entry(_)
                | Self::Since { .. }
        )
    }
}

/// What an operation returns. Serializable, for operations a standby
/// forwards to the primary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Outcome {
    Done,
    Value(#[serde(with = "crate::transport::wire::blobs::option")] Option<Vec<u8>>),
    Exists(bool),
    Keys(Vec<String>, Option<u64>),
    Count(u64),
    Values(
        #[serde(with = "crate::transport::wire::blobs::option_pairs")]
        Vec<Option<(String, Vec<u8>)>>,
    ),
    /// The bucket's revision, and the key's if it has one.
    Revision {
        bucket: u64,
        key: Option<u64>,
    },
    /// A value and its revision.
    Entry(#[serde(with = "crate::transport::wire::blobs::entry")] Option<(Vec<u8>, u64)>),
    /// The bucket's revision, the revision of its latest delete, and the
    /// matching keys changed since, with their revisions.
    Since {
        revision: u64,
        deleted: u64,
        changed: Vec<(String, u64)>,
    },
    /// A compare-and-swap wrote at this revision.
    Swapped(u64),
    /// A compare-and-swap found the key at another revision (`None`:
    /// absent) and changed nothing.
    Conflict(Option<u64>),
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Applied {
    pub(crate) outcome: Outcome,
    /// The bucket's size in bytes afterwards.
    pub(crate) bytes: u64,
    /// The revision the transaction changed the bucket to, if it did.
    pub(crate) revision: Option<u64>,
    /// Keys it wrote or deleted, in order, without repeats.
    pub(crate) changed: Vec<String>,
}

/// The view of one bucket an operation runs against.
pub(crate) trait Txn {
    fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, String>;
    /// Store `value` under `key`, stamped with `revision`.
    fn put(&mut self, key: &str, value: &[u8], revision: u64) -> Result<(), String>;
    /// Remove `key` and its revision.
    fn remove(&mut self, key: &str) -> Result<(), String>;
    /// The bucket's revision: 0 before its first change.
    fn revision(&mut self) -> Result<u64, String>;
    /// Record the bucket's revision after a change.
    fn set_revision(&mut self, revision: u64) -> Result<(), String>;
    /// The revision `key` was last written at.
    fn key_revision(&mut self, key: &str) -> Result<Option<u64>, String>;
    /// The revision of the bucket's latest delete: 0 if none.
    fn deleted(&mut self) -> Result<u64, String>;
    /// Record a delete at `revision`.
    fn set_deleted(&mut self, revision: u64) -> Result<(), String>;
    /// Keys in order, from `offset`, at most `limit`.
    fn keys(&mut self, offset: usize, limit: usize) -> Result<Vec<String>, String>;
    /// Bytes stored: the sum of key and value lengths.
    fn bytes(&mut self) -> Result<u64, String>;
}

fn entry_size(key: &str, value: &[u8]) -> u64 {
    u64::try_from(key.len() + value.len()).unwrap_or(u64::MAX)
}

/// The revision of an existing key: 0 if written before revisions.
fn current(txn: &mut dyn Txn, key: &str) -> Result<Option<u64>, String> {
    if txn.get(key)?.is_none() {
        return Ok(None);
    }
    Ok(Some(txn.key_revision(key)?.unwrap_or(0)))
}

/// Run `op` against `txn` as one transaction.
pub(crate) fn apply(txn: &mut dyn Txn, op: Op, quota: Option<u64>) -> Result<Applied, String> {
    let mut bytes = txn.bytes()?;
    let writes = !op.read_only();
    let revision = if writes {
        txn.revision()?.saturating_add(1)
    } else {
        0
    };
    let mut written: Vec<String> = Vec::new();
    let mut deleted = false;
    let outcome = match op {
        Op::Revision(key) => Outcome::Revision {
            bucket: txn.revision()?,
            key: txn.key_revision(&key)?,
        },
        Op::Entry(key) => match txn.get(&key)? {
            None => Outcome::Entry(None),
            Some(v) => Outcome::Entry(Some((v, txn.key_revision(&key)?.unwrap_or(0)))),
        },
        Op::Since { key, prefix, after } => {
            let mut changed = Vec::new();
            let keys = if prefix {
                txn.keys(0, usize::MAX)?
                    .into_iter()
                    .filter(|k| k.starts_with(&key))
                    .collect()
            } else {
                vec![key]
            };
            for k in keys {
                if let Some(r) = txn.key_revision(&k)?
                    && r > after
                {
                    changed.push((k, r));
                }
            }
            Outcome::Since {
                revision: txn.revision()?,
                deleted: txn.deleted()?,
                changed,
            }
        }
        Op::Swap {
            key,
            expected,
            value,
        } => {
            let found = current(txn, &key)?;
            if found == expected {
                bytes = set(txn, bytes, &key, &value, quota, revision)?;
                written.push(key);
                Outcome::Swapped(revision)
            } else {
                Outcome::Conflict(found)
            }
        }
        Op::SwapDelete { key, expected } => {
            let found = current(txn, &key)?;
            if found == Some(expected) {
                (bytes, _) = delete(txn, bytes, &key)?;
                written.push(key);
                deleted = true;
                Outcome::Swapped(revision)
            } else {
                Outcome::Conflict(found)
            }
        }
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
            bytes = set(txn, bytes, &key, &value, quota, revision)?;
            written.push(key);
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
            for (key, value) in pairs {
                bytes = set(txn, bytes, &key, &value, None, revision)?;
                written.push(key);
            }
            Outcome::Done
        }
        Op::Delete(key) => {
            let removed;
            (bytes, removed) = delete(txn, bytes, &key)?;
            if removed {
                written.push(key);
                deleted = true;
            }
            Outcome::Done
        }
        Op::DeleteMany(keys) => {
            for key in keys {
                let removed;
                (bytes, removed) = delete(txn, bytes, &key)?;
                if removed {
                    written.push(key);
                    deleted = true;
                }
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
            bytes = set(
                txn,
                bytes,
                &key,
                next.to_string().as_bytes(),
                quota,
                revision,
            )?;
            written.push(key);
            Outcome::Count(next)
        }
    };
    let changed = !written.is_empty();
    if changed {
        txn.set_revision(revision)?;
    }
    if deleted {
        txn.set_deleted(revision)?;
    }
    let mut seen = std::collections::HashSet::new();
    written.retain(|k| seen.insert(k.clone()));
    Ok(Applied {
        outcome,
        bytes,
        revision: changed.then_some(revision),
        changed: written,
    })
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
    revision: u64,
) -> Result<u64, String> {
    let old = txn.get(key)?.map_or(0, |v| entry_size(key, &v));
    let after = bytes
        .saturating_sub(old)
        .saturating_add(entry_size(key, value));
    check_quota(after, quota)?;
    txn.put(key, value, revision)?;
    Ok(after)
}

/// Remove `key`; returns the new size and whether the key existed.
fn delete(txn: &mut dyn Txn, bytes: u64, key: &str) -> Result<(u64, bool), String> {
    let Some(old) = txn.get(key)? else {
        return Ok((bytes, false));
    };
    txn.remove(key)?;
    Ok((bytes.saturating_sub(entry_size(key, &old)), true))
}

/// A memory bucket.
#[derive(Debug, Default)]
pub(crate) struct MemBucket {
    /// Values, and the revision each was written at.
    entries: BTreeMap<String, (Vec<u8>, u64)>,
    bytes: u64,
    revision: u64,
    deleted: u64,
}

impl Txn for MemBucket {
    fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.entries.get(key).map(|(v, _)| v.clone()))
    }

    fn put(&mut self, key: &str, value: &[u8], revision: u64) -> Result<(), String> {
        self.entries
            .insert(key.to_string(), (value.to_vec(), revision));
        Ok(())
    }

    fn remove(&mut self, key: &str) -> Result<(), String> {
        self.entries.remove(key);
        Ok(())
    }

    fn revision(&mut self) -> Result<u64, String> {
        Ok(self.revision)
    }

    fn set_revision(&mut self, revision: u64) -> Result<(), String> {
        self.revision = revision;
        Ok(())
    }

    fn key_revision(&mut self, key: &str) -> Result<Option<u64>, String> {
        Ok(self.entries.get(key).map(|(_, r)| *r))
    }

    fn deleted(&mut self) -> Result<u64, String> {
        Ok(self.deleted)
    }

    fn set_deleted(&mut self, revision: u64) -> Result<(), String> {
        self.deleted = revision;
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

/// A bucket in storage: its namespace (app or [`SHARED`]) and name.
pub(crate) type Key = (String, String);

/// Why a storage operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreError {
    /// A replicated bucket, and no primary can take the operation now (or
    /// a write's outcome is unknown: it may yet commit).
    Unavailable(String),
    /// Anything else: storage, a quota, a malformed counter.
    Other(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "unavailable: {why}"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

impl From<String> for StoreError {
    fn from(why: String) -> Self {
        Self::Other(why)
    }
}

impl From<&str> for StoreError {
    fn from(why: &str) -> Self {
        Self::Other(why.to_string())
    }
}

impl From<Refusal> for StoreError {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Unavailable => {
                Self::Unavailable("no primary with a lease can take it now".into())
            }
            Refusal::Unknown => Self::Unavailable(
                "not committed in time, or the primary stepped down; the write may yet commit"
                    .into(),
            ),
            Refusal::Failed(why) => Self::Other(why),
        }
    }
}

/// Every bucket on the node. Backends sit behind [`KvStore::run`], one per
/// durability: [`Memory`], [`Disk`], and replicated shared buckets through
/// [`crate::replication`]. Each runs [`apply`] in one transaction, so
/// guests see the same rules from all of them.
#[derive(Debug)]
pub(crate) struct KvStore {
    memory: Memory,
    disk: Option<Arc<Disk>>,
    /// Shared buckets from node configuration, by name.
    shared: BTreeMap<String, SharedBucket>,
    /// Watches, told about every change after it commits.
    pub(crate) watches: Watches,
    /// Replication, once started: its task, and where it stands.
    replication: OnceLock<(Handle, watch::Receiver<ReplicationStatus>)>,
}

/// The memory backend.
#[derive(Debug, Default)]
struct Memory {
    buckets: Mutex<HashMap<Key, MemBucket>>,
}

impl Memory {
    fn run(&self, key: Key, op: Op, quota: Option<u64>) -> Result<Applied, String> {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let b = buckets.entry(key).or_default();
        let applied = apply(b, op, quota)?;
        b.bytes = applied.bytes;
        Ok(applied)
    }
}

/// The disk backend: one redb file, which also holds a replicated bucket's
/// log ([`crate::replication`]).
#[derive(Debug)]
pub(crate) struct Disk {
    db: redb::Database,
    /// Bucket sizes, computed on first use and kept after each commit.
    sizes: Mutex<HashMap<Key, u64>>,
}

impl KvStore {
    /// Open the store; disk buckets need `data_dir`.
    pub(crate) fn open(
        data_dir: Option<&Path>,
        shared: BTreeMap<String, SharedBucket>,
    ) -> Result<Self, String> {
        if data_dir.is_none()
            && let Some((name, _)) = shared
                .iter()
                .find(|(_, b)| b.durability != Durability::Memory)
        {
            return Err(format!(
                "shared bucket '{name}' is on disk, and the node has no data directory"
            ));
        }
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
            memory: Memory::default(),
            disk,
            shared,
            watches: Watches::default(),
            replication: OnceLock::new(),
        })
    }

    /// Whether the node configures replicated shared buckets.
    pub(crate) fn has_replicated(&self) -> bool {
        self.shared
            .values()
            .any(|b| b.durability == Durability::Replicated)
    }

    /// Route replicated buckets through `handle`; once only.
    pub(crate) fn set_replication(
        &self,
        handle: Handle,
        status: watch::Receiver<ReplicationStatus>,
    ) -> Result<(), ()> {
        self.replication.set((handle, status)).map_err(|_| ())
    }

    /// The replication task, once started.
    pub(crate) fn replication(&self) -> Option<&Handle> {
        self.replication.get().map(|(h, _)| h)
    }

    /// Where replication stands, once started.
    pub(crate) fn replication_status(&self) -> Option<watch::Receiver<ReplicationStatus>> {
        self.replication.get().map(|(_, s)| s.clone())
    }

    /// The shared bucket `name`, if the node configures it.
    pub(crate) fn shared(&self, name: &str) -> Option<&SharedBucket> {
        self.shared.get(name)
    }

    /// The disk backend, if the node has a data directory.
    pub(crate) fn disk(&self) -> Option<Arc<Disk>> {
        self.disk.clone()
    }

    /// Run one operation against a bucket. A write to a replicated bucket
    /// returns once it commits, and its watches hear of it from
    /// replication, on every node; a read is local.
    pub(crate) async fn run(&self, bucket: &BucketHandle, op: Op) -> Result<Outcome, StoreError> {
        if bucket.space == SPACE && !op.read_only() {
            let handle = self
                .replication()
                .ok_or_else(|| StoreError::Unavailable("replication is not running".into()))?;
            return Ok(handle.write(&bucket.name, bucket.quota, op).await?);
        }
        let key = (bucket.space.clone(), bucket.name.clone());
        let applied = self.apply(bucket, key.clone(), op).await?;
        if let Some(revision) = applied.revision {
            self.watches.notify(&key, &applied.changed, revision);
        }
        Ok(applied.outcome)
    }

    /// A read that reflects every write committed before it: on a
    /// replicated bucket, served by the primary (spike S20's R5); on any
    /// other, as [`KvStore::run`].
    pub(crate) async fn run_linearizable(
        &self,
        bucket: &BucketHandle,
        op: Op,
    ) -> Result<Outcome, StoreError> {
        if bucket.space != SPACE {
            return self.run(bucket, op).await;
        }
        let handle = self
            .replication()
            .ok_or_else(|| StoreError::Unavailable("replication is not running".into()))?;
        Ok(handle.read(&bucket.name, op).await?)
    }

    /// Run `op` on the bucket's backend.
    async fn apply(&self, bucket: &BucketHandle, key: Key, op: Op) -> Result<Applied, String> {
        if bucket.space == SPACE {
            // A replicated bucket's local copy, for reads.
            let disk = self
                .disk
                .clone()
                .ok_or("disk storage is not configured on this node")?;
            return disk.run(&key, op, bucket.quota);
        }
        match bucket.durability {
            Durability::Memory => self.memory.run(key, op, bucket.quota),
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
                Err("replicated app buckets are not supported yet (ROADMAP K3)".into())
            }
        }
    }
}

/// Each bucket's revision, by table name. Table names cannot clash with
/// buckets' (`<space>/<bucket>`), which always hold a `/`.
const REVISIONS: TableDefinition<&str, u64> = TableDefinition::new("@revisions");

/// The `@revisions` key holding a bucket's latest delete.
fn deleted_key(table: &str) -> String {
    format!("{table}#deleted")
}

/// A table in a read transaction, or `None` if nothing has written it yet.
fn open_read<K: redb::Key + 'static, V: redb::Value + 'static>(
    txn: &redb::ReadTransaction,
    table: TableDefinition<K, V>,
) -> Result<Option<redb::ReadOnlyTable<K, V>>, String> {
    match txn.open_table(table) {
        Ok(t) => Ok(Some(t)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

/// One bucket's whole contents, for a snapshot of a replicated bucket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BucketDump {
    pub(crate) name: String,
    /// The bucket's revision and its latest delete's.
    pub(crate) revision: u64,
    pub(crate) deleted: u64,
    /// Keys, values and their revisions, in key order.
    pub(crate) entries: Vec<DumpEntry>,
}

/// One key of a [`BucketDump`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DumpEntry {
    pub(crate) key: String,
    #[serde(with = "crate::transport::wire::bytes")]
    pub(crate) value: Vec<u8>,
    pub(crate) revision: u64,
}

impl Disk {
    /// An in-memory database, for tests.
    #[cfg(test)]
    pub(crate) fn in_memory() -> Self {
        let db = redb::Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .unwrap_or_else(|e| panic!("in-memory database: {e}"));
        Self {
            db,
            sizes: Mutex::default(),
        }
    }

    /// The database, for transactions spanning buckets and the replicated
    /// log.
    pub(crate) fn db(&self) -> &redb::Database {
        &self.db
    }

    /// Run a write `op` on bucket `key` inside `txn`, which the caller
    /// commits. The cached size is updated at once; after an abort, call
    /// [`Disk::forget_sizes`].
    pub(crate) fn apply_in(
        &self,
        txn: &redb::WriteTransaction,
        key: &Key,
        op: Op,
        quota: Option<u64>,
    ) -> Result<Applied, String> {
        let name = format!("{}/{}", key.0, key.1);
        let rev_name = format!("{name}#rev");
        let table: TableDefinition<&str, &[u8]> = TableDefinition::new(&name);
        let revs: TableDefinition<&str, u64> = TableDefinition::new(&rev_name);
        let cached = self
            .sizes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .copied();
        let e = |err: &dyn std::fmt::Display| err.to_string();
        let applied = {
            let mut view = DiskWrite {
                table: txn.open_table(table).map_err(|err| e(&err))?,
                revs: txn.open_table(revs).map_err(|err| e(&err))?,
                buckets: txn.open_table(REVISIONS).map_err(|err| e(&err))?,
                name: &name,
                cached,
            };
            apply(&mut view, op, quota)?
        };
        self.remember(key, applied.bytes);
        Ok(applied)
    }

    /// Drop the cached sizes of every bucket in `space`, after a
    /// transaction that changed them was aborted or replaced them.
    pub(crate) fn forget_sizes(&self, space: &str) {
        self.sizes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(s, _), _| s != space);
    }

    /// Every bucket in `space`, as `txn` sees it.
    pub(crate) fn export(
        &self,
        txn: &redb::ReadTransaction,
        space: &str,
    ) -> Result<Vec<BucketDump>, String> {
        let e = |err: &dyn std::fmt::Display| err.to_string();
        let prefix = format!("{space}/");
        let mut names: Vec<String> = txn
            .list_tables()
            .map_err(|err| e(&err))?
            .map(|t| t.name().to_string())
            .filter(|n| n.starts_with(&prefix) && !n.ends_with("#rev"))
            .collect();
        names.sort();
        let buckets = open_read(txn, REVISIONS)?;
        let mut dumps = Vec::with_capacity(names.len());
        for name in names {
            let table: TableDefinition<&str, &[u8]> = TableDefinition::new(&name);
            let rev_name = format!("{name}#rev");
            let revs: TableDefinition<&str, u64> = TableDefinition::new(&rev_name);
            let revs = open_read(txn, revs)?;
            let mut entries = Vec::new();
            if let Some(t) = open_read(txn, table)? {
                for entry in t.iter().map_err(|err| e(&err))? {
                    let (k, v) = entry.map_err(|err| e(&err))?;
                    let key = k.value().to_string();
                    let revision = read_u64(revs.as_ref(), &key)?.unwrap_or(0);
                    entries.push(DumpEntry {
                        key,
                        value: v.value().to_vec(),
                        revision,
                    });
                }
            }
            dumps.push(BucketDump {
                revision: read_u64(buckets.as_ref(), &name)?.unwrap_or(0),
                deleted: read_u64(buckets.as_ref(), &deleted_key(&name))?.unwrap_or(0),
                name: name[prefix.len()..].to_string(),
                entries,
            });
        }
        Ok(dumps)
    }

    /// Replace every bucket in `space` with `dumps`, inside `txn`, which
    /// the caller commits; then [`Disk::forget_sizes`].
    pub(crate) fn import(
        &self,
        txn: &redb::WriteTransaction,
        space: &str,
        dumps: &[BucketDump],
    ) -> Result<(), String> {
        let e = |err: &dyn std::fmt::Display| err.to_string();
        let prefix = format!("{space}/");
        let old: Vec<String> = txn
            .list_tables()
            .map_err(|err| e(&err))?
            .map(|t| t.name().to_string())
            .filter(|n| n.starts_with(&prefix))
            .collect();
        let mut buckets = txn.open_table(REVISIONS).map_err(|err| e(&err))?;
        for name in &old {
            // Values and revisions have different types; delete both ways.
            let values: TableDefinition<&str, &[u8]> = TableDefinition::new(name);
            let revs: TableDefinition<&str, u64> = TableDefinition::new(name);
            if name.ends_with("#rev") {
                txn.delete_table(revs).map_err(|err| e(&err))?;
            } else {
                txn.delete_table(values).map_err(|err| e(&err))?;
                buckets.remove(name.as_str()).map_err(|err| e(&err))?;
                buckets
                    .remove(deleted_key(name).as_str())
                    .map_err(|err| e(&err))?;
            }
        }
        for dump in dumps {
            let name = format!("{prefix}{}", dump.name);
            let rev_name = format!("{name}#rev");
            let values: TableDefinition<&str, &[u8]> = TableDefinition::new(&name);
            let revs: TableDefinition<&str, u64> = TableDefinition::new(&rev_name);
            let mut values = txn.open_table(values).map_err(|err| e(&err))?;
            let mut revs = txn.open_table(revs).map_err(|err| e(&err))?;
            for entry in &dump.entries {
                values
                    .insert(entry.key.as_str(), entry.value.as_slice())
                    .map_err(|err| e(&err))?;
                revs.insert(entry.key.as_str(), entry.revision)
                    .map_err(|err| e(&err))?;
            }
            if dump.revision > 0 {
                buckets
                    .insert(name.as_str(), dump.revision)
                    .map_err(|err| e(&err))?;
            }
            if dump.deleted > 0 {
                buckets
                    .insert(deleted_key(&name).as_str(), dump.deleted)
                    .map_err(|err| e(&err))?;
            }
        }
        Ok(())
    }

    /// Run `op` on bucket `key` in its own transaction.
    pub(crate) fn run(&self, key: &Key, op: Op, quota: Option<u64>) -> Result<Applied, String> {
        // A bucket is two tables: values, and each key's revision.
        let name = format!("{}/{}", key.0, key.1);
        let rev_name = format!("{name}#rev");
        let table: TableDefinition<&str, &[u8]> = TableDefinition::new(&name);
        let revs: TableDefinition<&str, u64> = TableDefinition::new(&rev_name);
        let cached = self
            .sizes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .copied();
        let e = |err: &dyn std::fmt::Display| err.to_string();
        if op.read_only() {
            let txn = self.db.begin_read().map_err(|err| e(&err))?;
            let mut view = DiskRead {
                table: open_read(&txn, table)?,
                revs: open_read(&txn, revs)?,
                buckets: open_read(&txn, REVISIONS)?,
                name: &name,
                cached,
            };
            let applied = apply(&mut view, op, quota)?;
            self.remember(key, applied.bytes);
            return Ok(applied);
        }
        let txn = self.db.begin_write().map_err(|err| e(&err))?;
        let applied = {
            let mut view = DiskWrite {
                table: txn.open_table(table).map_err(|err| e(&err))?,
                revs: txn.open_table(revs).map_err(|err| e(&err))?,
                buckets: txn.open_table(REVISIONS).map_err(|err| e(&err))?,
                name: &name,
                cached,
            };
            apply(&mut view, op, quota)?
        };
        if applied.revision.is_none() {
            // Nothing changed (a compare-and-swap conflict, a missing
            // delete): no commit, no fsync.
            txn.abort().map_err(|err| e(&err))?;
            return Ok(applied);
        }
        txn.commit().map_err(|err| e(&err))?;
        self.remember(key, applied.bytes);
        Ok(applied)
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

struct DiskRead<'a> {
    table: Option<redb::ReadOnlyTable<&'static str, &'static [u8]>>,
    revs: Option<redb::ReadOnlyTable<&'static str, u64>>,
    buckets: Option<redb::ReadOnlyTable<&'static str, u64>>,
    name: &'a str,
    cached: Option<u64>,
}

fn read_u64<T: ReadableTable<&'static str, u64>>(
    table: Option<&T>,
    key: &str,
) -> Result<Option<u64>, String> {
    let Some(t) = table else {
        return Ok(None);
    };
    Ok(t.get(key).map_err(|e| e.to_string())?.map(|v| v.value()))
}

impl Txn for DiskRead<'_> {
    fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(t) = &self.table else {
            return Ok(None);
        };
        Ok(t.get(key)
            .map_err(|e| e.to_string())?
            .map(|v| v.value().to_vec()))
    }

    fn put(&mut self, _key: &str, _value: &[u8], _revision: u64) -> Result<(), String> {
        Err("write in a read transaction".into())
    }

    fn remove(&mut self, _key: &str) -> Result<(), String> {
        Err("write in a read transaction".into())
    }

    fn revision(&mut self) -> Result<u64, String> {
        Ok(read_u64(self.buckets.as_ref(), self.name)?.unwrap_or(0))
    }

    fn set_revision(&mut self, _revision: u64) -> Result<(), String> {
        Err("write in a read transaction".into())
    }

    fn key_revision(&mut self, key: &str) -> Result<Option<u64>, String> {
        read_u64(self.revs.as_ref(), key)
    }

    fn deleted(&mut self) -> Result<u64, String> {
        Ok(read_u64(self.buckets.as_ref(), &deleted_key(self.name))?.unwrap_or(0))
    }

    fn set_deleted(&mut self, _revision: u64) -> Result<(), String> {
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

struct DiskWrite<'txn, 'a> {
    table: redb::Table<'txn, &'static str, &'static [u8]>,
    revs: redb::Table<'txn, &'static str, u64>,
    buckets: redb::Table<'txn, &'static str, u64>,
    name: &'a str,
    cached: Option<u64>,
}

impl Txn for DiskWrite<'_, '_> {
    fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .table
            .get(key)
            .map_err(|e| e.to_string())?
            .map(|v| v.value().to_vec()))
    }

    fn put(&mut self, key: &str, value: &[u8], revision: u64) -> Result<(), String> {
        self.table.insert(key, value).map_err(|e| e.to_string())?;
        self.revs.insert(key, revision).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn remove(&mut self, key: &str) -> Result<(), String> {
        self.table.remove(key).map_err(|e| e.to_string())?;
        self.revs.remove(key).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn revision(&mut self) -> Result<u64, String> {
        Ok(read_u64(Some(&self.buckets), self.name)?.unwrap_or(0))
    }

    fn set_revision(&mut self, revision: u64) -> Result<(), String> {
        self.buckets
            .insert(self.name, revision)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn key_revision(&mut self, key: &str) -> Result<Option<u64>, String> {
        read_u64(Some(&self.revs), key)
    }

    fn deleted(&mut self) -> Result<u64, String> {
        Ok(read_u64(Some(&self.buckets), &deleted_key(self.name))?.unwrap_or(0))
    }

    fn set_deleted(&mut self, revision: u64) -> Result<(), String> {
        self.buckets
            .insert(deleted_key(self.name).as_str(), revision)
            .map_err(|e| e.to_string())?;
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
