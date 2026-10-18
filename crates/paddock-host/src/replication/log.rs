//! The replicated log on disk, in the node's keyvalue database next to the
//! buckets it feeds, so applying an entry and recording the commit index
//! are one transaction.
//!
//! Entries run from `base + 1` to `last`; entries up to `base` were
//! compacted (or came in a snapshot), and `base_epoch` is the epoch of the
//! entry at `base`. Every entry up to `commit` has been applied to the
//! `@cluster` buckets. Each change here is one redb transaction. Appends,
//! compaction and snapshots are synced before they return: an entry is on
//! disk before it is acknowledged or committed (spike S20's R3). Applying
//! committed entries is not synced: the entries are on disk already, and
//! the commit index is kept in the same transaction as the data it covers,
//! so a crash loses both together and the node applies those entries
//! again, alike, once it learns the commit index anew (a lower commit
//! index after a crash, as the model allows). Every later synced
//! transaction makes the applies before it durable.

use std::sync::Arc;
use std::time::Duration;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::services::keyvalue::storage::{Applied, BucketDump, Disk, Op};

/// The namespace of replicated buckets, apart from node-scoped shared
/// buckets (`@shared`): a bucket whose scope changes starts empty rather
/// than with one node's data.
pub(crate) const SPACE: &str = "@cluster";

/// Entries by index.
const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("@replication#log");
/// `base`, `base_epoch` and `commit`.
const META: TableDefinition<&str, u64> = TableDefinition::new("@replication#meta");

/// One log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    /// The epoch of the primary that appended it.
    pub(crate) epoch: u64,
    pub(crate) cmd: Command,
}

/// What an entry does once committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Command {
    /// A new primary's first entry (R4).
    Noop,
    /// A write to a replicated bucket, with the bucket's quota as the
    /// primary had it, so every node applies it alike.
    Write {
        bucket: String,
        quota: Option<u64>,
        op: Op,
    },
}

impl Entry {
    /// Its encoded size, for batching.
    pub(crate) fn size(&self) -> usize {
        match &self.cmd {
            Command::Noop => 16,
            Command::Write { bucket, op, .. } => 32 + bucket.len() + op_size(op),
        }
    }
}

fn op_size(op: &Op) -> usize {
    match op {
        Op::Set(k, v) => k.len() + v.len(),
        Op::SetMany(pairs) => pairs.iter().map(|(k, v)| k.len() + v.len() + 8).sum(),
        Op::DeleteMany(keys) | Op::GetMany(keys) => keys.iter().map(|k| k.len() + 8).sum(),
        Op::Swap { key, value, .. } => key.len() + value.len() + 16,
        Op::Get(k)
        | Op::Delete(k)
        | Op::Exists(k)
        | Op::Increment(k, _)
        | Op::Revision(k)
        | Op::Entry(k)
        | Op::SwapDelete { key: k, .. }
        | Op::Since { key: k, .. } => k.len() + 16,
        Op::Keys(_) => 16,
    }
}

/// The `@cluster` buckets as of a commit index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// The commit index it reflects.
    pub(crate) index: u64,
    /// The epoch of the entry at `index`.
    pub(crate) epoch: u64,
    pub(crate) buckets: Vec<BucketDump>,
}

/// An entry applied by [`Log::commit_to`].
#[derive(Debug)]
pub(crate) struct Committed {
    pub(crate) index: u64,
    /// The bucket a write went to, and what it did; `None` for a no-op.
    pub(crate) write: Option<(String, Result<Applied, String>)>,
}

/// What the log remembers in memory of each entry it holds.
#[derive(Debug, Clone, Copy)]
struct Held {
    epoch: u64,
    /// Its encoded size.
    bytes: u64,
    /// When this node appended it (when it opened the log, for entries it
    /// found there).
    at: Instant,
}

/// The log, with what it holds cached in memory.
pub(crate) struct Log {
    disk: Arc<Disk>,
    base: u64,
    base_epoch: u64,
    /// Entries `base + 1 ..= last`.
    held: Vec<Held>,
    /// The sum of their sizes.
    bytes: u64,
    commit: u64,
}

impl std::fmt::Debug for Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Log")
            .field("base", &self.base)
            .field("last", &self.last())
            .field("commit", &self.commit)
            .finish_non_exhaustive()
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn encode(entry: &Entry) -> Result<Vec<u8>, String> {
    crate::transport::wire::to_cbor(entry).map_err(err)
}

fn decode(bytes: &[u8]) -> Result<Entry, String> {
    crate::transport::wire::from_cbor(bytes).map_err(err)
}

impl Log {
    /// The log as kept in `disk`'s database.
    pub(crate) fn open(disk: Arc<Disk>) -> Result<Self, String> {
        let txn = disk.db().begin_read().map_err(err)?;
        let (base, base_epoch, commit) = match txn.open_table(META) {
            Ok(meta) => {
                let get = |k: &str| -> Result<u64, String> {
                    Ok(meta.get(k).map_err(err)?.map_or(0, |v| v.value()))
                };
                (get("base")?, get("base_epoch")?, get("commit")?)
            }
            Err(redb::TableError::TableDoesNotExist(_)) => (0, 0, 0),
            Err(e) => return Err(err(e)),
        };
        let mut held = Vec::new();
        let now = Instant::now();
        match txn.open_table(LOG) {
            Ok(log) => {
                for (expected, item) in (base + 1..).zip(log.range(base + 1..).map_err(err)?) {
                    let (k, v) = item.map_err(err)?;
                    if k.value() != expected {
                        return Err(format!(
                            "replicated log: entry {} follows {}",
                            k.value(),
                            expected - 1
                        ));
                    }
                    held.push(Held {
                        epoch: decode(v.value())?.epoch,
                        bytes: v.value().len() as u64,
                        at: now,
                    });
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(err(e)),
        }
        Ok(Self {
            disk,
            base,
            base_epoch,
            bytes: held.iter().map(|h| h.bytes).sum(),
            held,
            commit,
        })
    }

    /// The disk backend the log and its buckets live in.
    pub(crate) fn disk(&self) -> &Arc<Disk> {
        &self.disk
    }

    pub(crate) fn base(&self) -> u64 {
        self.base
    }

    pub(crate) fn last(&self) -> u64 {
        self.base + self.held.len() as u64
    }

    pub(crate) fn commit(&self) -> u64 {
        self.commit
    }

    /// The epoch of the entry at `index`: 0 for index 0, `None` once
    /// compacted or past the end.
    pub(crate) fn epoch_at(&self, index: u64) -> Option<u64> {
        if index == 0 {
            return Some(0);
        }
        if index == self.base {
            return Some(self.base_epoch);
        }
        if index < self.base {
            return None;
        }
        let i = usize::try_from(index - self.base - 1).ok()?;
        self.held.get(i).map(|h| h.epoch)
    }

    /// The epoch of the last entry.
    pub(crate) fn last_epoch(&self) -> u64 {
        self.held.last().map_or(self.base_epoch, |h| h.epoch)
    }

    /// Write `entries` from index `from`, dropping whatever the log held
    /// from there on. `from` must be past the commit index and at most one
    /// past the end.
    pub(crate) fn append(&mut self, from: u64, entries: &[Entry]) -> Result<(), String> {
        if from <= self.commit || from > self.last() + 1 || from <= self.base {
            return Err(format!(
                "replicated log: append at {from} with commit {} and last {}",
                self.commit,
                self.last()
            ));
        }
        let txn = self.disk.db().begin_write().map_err(err)?;
        let mut added = Vec::with_capacity(entries.len());
        let now = Instant::now();
        {
            let mut log = txn.open_table(LOG).map_err(err)?;
            for index in from..=self.last() {
                log.remove(index).map_err(err)?;
            }
            for (i, entry) in entries.iter().enumerate() {
                let bytes = encode(entry)?;
                log.insert(from + i as u64, bytes.as_slice()).map_err(err)?;
                added.push(Held {
                    epoch: entry.epoch,
                    bytes: bytes.len() as u64,
                    at: now,
                });
            }
        }
        txn.commit().map_err(err)?;
        let dropped = self
            .held
            .drain(usize::try_from(from - self.base - 1).unwrap_or(usize::MAX)..);
        self.bytes -= dropped.map(|h| h.bytes).sum::<u64>();
        self.bytes += added.iter().map(|h| h.bytes).sum::<u64>();
        self.held.extend(added);
        Ok(())
    }

    /// Entries from `from`, at most `max` of them and about `max_bytes`
    /// (at least one).
    pub(crate) fn entries(
        &self,
        from: u64,
        max: usize,
        max_bytes: usize,
    ) -> Result<Vec<Entry>, String> {
        if from <= self.base {
            return Err(format!("replicated log: entry {from} is compacted"));
        }
        let txn = self.disk.db().begin_read().map_err(err)?;
        let log = txn.open_table(LOG).map_err(err)?;
        let mut out = Vec::new();
        let mut bytes = 0;
        for item in log.range(from..).map_err(err)? {
            let (_, v) = item.map_err(err)?;
            let entry = decode(v.value())?;
            bytes += entry.size();
            if !out.is_empty() && (out.len() >= max || bytes > max_bytes) {
                break;
            }
            out.push(entry);
        }
        Ok(out)
    }

    /// Apply entries up to `to` to the buckets and raise the commit index
    /// to it, in one transaction. Each write's own failure (a quota, a
    /// counter that is not a number) is its outcome; nothing else
    /// changes for it, on every node alike.
    pub(crate) fn commit_to(&mut self, to: u64) -> Result<Vec<Committed>, String> {
        if to <= self.commit {
            return Ok(Vec::new());
        }
        if to > self.last() {
            return Err(format!(
                "replicated log: commit to {to} past the last entry {}",
                self.last()
            ));
        }
        let mut txn = self.disk.db().begin_write().map_err(err)?;
        // Not synced: see the module's documentation.
        txn.set_durability(redb::Durability::None).map_err(err)?;
        let result = self.apply_range(&txn, to);
        let committed = match result {
            Ok(c) => c,
            Err(e) => {
                let _ = txn.abort();
                self.disk.forget_sizes(SPACE);
                return Err(e);
            }
        };
        if let Err(e) = txn.commit() {
            self.disk.forget_sizes(SPACE);
            return Err(err(e));
        }
        self.commit = to;
        Ok(committed)
    }

    fn apply_range(&self, txn: &redb::WriteTransaction, to: u64) -> Result<Vec<Committed>, String> {
        let mut committed = Vec::new();
        {
            let log = txn.open_table(LOG).map_err(err)?;
            for index in self.commit + 1..=to {
                let bytes = log
                    .get(index)
                    .map_err(err)?
                    .ok_or_else(|| format!("replicated log: entry {index} missing"))?
                    .value()
                    .to_vec();
                let write = match decode(&bytes)?.cmd {
                    Command::Noop => None,
                    Command::Write { bucket, quota, op } => {
                        let key = (SPACE.to_string(), bucket.clone());
                        Some((bucket, self.disk.apply_in(txn, &key, op, quota)))
                    }
                };
                committed.push(Committed { index, write });
            }
        }
        let mut meta = txn.open_table(META).map_err(err)?;
        meta.insert("commit", to).map_err(err)?;
        Ok(committed)
    }

    /// How far to compact for the log to hold at most `max_bytes` and no
    /// entry older than `max_age`: past the oldest entries until both
    /// hold, but never past the commit index. Within both bounds nothing
    /// is dropped, so a standby back from an outage catches up from the
    /// log; past them, from a snapshot.
    pub(crate) fn compact_target(&self, now: Instant, max_bytes: u64, max_age: Duration) -> u64 {
        let mut to = self.base;
        let mut bytes = self.bytes;
        for h in &self.held {
            if to >= self.commit
                || (bytes <= max_bytes && now.saturating_duration_since(h.at) <= max_age)
            {
                break;
            }
            to += 1;
            bytes -= h.bytes;
        }
        to
    }

    /// The encoded size of the entries the log holds.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Drop entries up to `to`, which must be committed.
    pub(crate) fn compact(&mut self, to: u64) -> Result<(), String> {
        if to <= self.base {
            return Ok(());
        }
        if to > self.commit {
            return Err(format!(
                "replicated log: compact to {to} past the commit index {}",
                self.commit
            ));
        }
        let epoch = self
            .epoch_at(to)
            .ok_or_else(|| format!("replicated log: no entry {to}"))?;
        let txn = self.disk.db().begin_write().map_err(err)?;
        {
            let mut log = txn.open_table(LOG).map_err(err)?;
            for index in self.base + 1..=to {
                log.remove(index).map_err(err)?;
            }
            let mut meta = txn.open_table(META).map_err(err)?;
            meta.insert("base", to).map_err(err)?;
            meta.insert("base_epoch", epoch).map_err(err)?;
        }
        txn.commit().map_err(err)?;
        let dropped = self
            .held
            .drain(..usize::try_from(to - self.base).unwrap_or(usize::MAX));
        self.bytes -= dropped.map(|h| h.bytes).sum::<u64>();
        self.base = to;
        self.base_epoch = epoch;
        Ok(())
    }

    /// The buckets at the commit index, read in one transaction.
    pub(crate) fn snapshot(&self) -> Result<Snapshot, String> {
        let txn = self.disk.db().begin_read().map_err(err)?;
        let buckets = self.disk.export(&txn, SPACE)?;
        Ok(Snapshot {
            index: self.commit,
            epoch: self
                .epoch_at(self.commit)
                .ok_or("replicated log: the commit index is compacted past")?,
            buckets,
        })
    }

    /// Replace the buckets and the whole log with `snap`: base and commit
    /// at its index.
    pub(crate) fn install(&mut self, snap: &Snapshot) -> Result<(), String> {
        let txn = self.disk.db().begin_write().map_err(err)?;
        let result = (|| {
            self.disk.import(&txn, SPACE, &snap.buckets)?;
            let mut log = txn.open_table(LOG).map_err(err)?;
            for index in self.base + 1..=self.last() {
                log.remove(index).map_err(err)?;
            }
            let mut meta = txn.open_table(META).map_err(err)?;
            meta.insert("base", snap.index).map_err(err)?;
            meta.insert("base_epoch", snap.epoch).map_err(err)?;
            meta.insert("commit", snap.index).map_err(err)?;
            Ok::<(), String>(())
        })();
        self.disk.forget_sizes(SPACE);
        match result {
            Ok(()) => txn.commit().map_err(err)?,
            Err(e) => {
                let _ = txn.abort();
                return Err(e);
            }
        }
        self.base = snap.index;
        self.base_epoch = snap.epoch;
        self.commit = snap.index;
        self.held.clear();
        self.bytes = 0;
        Ok(())
    }
}

#[cfg(test)]
#[path = "log_tests.rs"]
mod log_tests;
