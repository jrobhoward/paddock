//! The replicated log on disk, in the node's keyvalue database next to the
//! buckets it feeds, so applying an entry and recording the commit index
//! are one transaction.
//!
//! Entries run from `base + 1` to `last`; entries up to `base` were
//! compacted (or came in a snapshot), and `base_epoch` is the epoch of the
//! entry at `base`. Every entry up to `commit` has been applied to the
//! `@cluster` buckets. Each change here is one redb transaction, synced
//! before it returns: an entry is on disk before it is acknowledged or
//! committed (spike S20's R3), and the commit index survives a crash with
//! the data it covers.

use std::sync::Arc;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

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

/// The log, with what it holds cached in memory.
pub(crate) struct Log {
    disk: Arc<Disk>,
    base: u64,
    base_epoch: u64,
    /// The epochs of entries `base + 1 ..= last`.
    epochs: Vec<u64>,
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
        let mut epochs = Vec::new();
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
                    epochs.push(decode(v.value())?.epoch);
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(err(e)),
        }
        Ok(Self {
            disk,
            base,
            base_epoch,
            epochs,
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
        self.base + self.epochs.len() as u64
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
        self.epochs.get(i).copied()
    }

    /// The epoch of the last entry.
    pub(crate) fn last_epoch(&self) -> u64 {
        self.epochs.last().copied().unwrap_or(self.base_epoch)
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
        {
            let mut log = txn.open_table(LOG).map_err(err)?;
            for index in from..=self.last() {
                log.remove(index).map_err(err)?;
            }
            for (i, entry) in entries.iter().enumerate() {
                let bytes = encode(entry)?;
                log.insert(from + i as u64, bytes.as_slice()).map_err(err)?;
            }
        }
        txn.commit().map_err(err)?;
        self.epochs
            .truncate(usize::try_from(from - self.base - 1).unwrap_or(usize::MAX));
        self.epochs.extend(entries.iter().map(|e| e.epoch));
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
        let txn = self.disk.db().begin_write().map_err(err)?;
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
        self.epochs
            .drain(..usize::try_from(to - self.base).unwrap_or(usize::MAX));
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
        self.epochs.clear();
        Ok(())
    }
}

#[cfg(test)]
#[path = "log_tests.rs"]
mod log_tests;
