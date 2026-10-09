//! Watches on keyvalue buckets (`paddock:watch`).
//!
//! A watch names a bucket (by its storage key, so app buckets and shared
//! buckets are told apart), a key or prefix, and the watching instance.
//! After a transaction commits, [`Watches::notify`] tells every matching
//! watch about each key it changed. Notices are conflated per key: a watch
//! keeps the latest revision of each key whose notice is still queued, and
//! queues a notice only for a key with none waiting. The notice travels as
//! an [`Info::Late`] on the instance's signal queue (unbounded, so never
//! dropped, and bounded in practice by the keys a watch can match), and
//! reads the latest revision when the instance takes it.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::instance::InstanceHandle;
use crate::message::{Envelope, Info, Late, LateEvent};
use crate::pid::Pid;

use super::storage::Key;

/// The host event a notice arrives as.
pub(crate) const EVENT: &str = "kv-changed";

/// Every watch on the node.
#[derive(Debug, Default)]
pub(crate) struct Watches {
    /// Watches alive, so a write with none to tell skips the table.
    active: AtomicUsize,
    table: Mutex<Table>,
}

#[derive(Debug, Default)]
struct Table {
    next: u64,
    /// Watches on each bucket.
    by_bucket: HashMap<Key, BucketWatches>,
    /// Each instance's watches, by id, with their bucket.
    by_owner: HashMap<Pid, BTreeMap<u64, (Key, Arc<Watch>)>>,
}

/// One bucket's watches: by key for exact ones, so a write touches only the
/// watches on the keys it changed, and every prefix watch.
#[derive(Debug, Default)]
struct BucketWatches {
    exact: HashMap<String, BTreeMap<u64, Arc<Watch>>>,
    prefix: BTreeMap<u64, Arc<Watch>>,
}

impl BucketWatches {
    fn insert(&mut self, watch: Arc<Watch>) {
        if watch.prefix {
            self.prefix.insert(watch.id, watch);
        } else {
            self.exact
                .entry(watch.key.clone())
                .or_default()
                .insert(watch.id, watch);
        }
    }

    fn remove(&mut self, watch: &Watch) {
        if watch.prefix {
            self.prefix.remove(&watch.id);
        } else if let Some(ws) = self.exact.get_mut(&watch.key) {
            ws.remove(&watch.id);
            if ws.is_empty() {
                self.exact.remove(&watch.key);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.prefix.is_empty()
    }
}

/// One watch.
#[derive(Debug)]
pub(crate) struct Watch {
    pub(crate) id: u64,
    /// The bucket's name as the guest opened it.
    bucket: String,
    key: String,
    prefix: bool,
    handle: InstanceHandle,
    /// Keys with a notice queued, and their latest revision.
    pending: Mutex<HashMap<String, u64>>,
    ended: AtomicBool,
}

/// Why a watch could not start.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LimitReached;

impl Watches {
    /// Start a watch for `handle` on `bucket`, unless it holds `max`
    /// already.
    pub(crate) fn add(
        &self,
        storage: Key,
        bucket: &str,
        key: &str,
        prefix: bool,
        handle: &InstanceHandle,
        max: usize,
    ) -> Result<Arc<Watch>, LimitReached> {
        let mut t = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        let owner = handle.pid();
        if t.by_owner.get(&owner).map_or(0, BTreeMap::len) >= max {
            return Err(LimitReached);
        }
        t.next += 1;
        let watch = Arc::new(Watch {
            id: t.next,
            bucket: bucket.to_string(),
            key: key.to_string(),
            prefix,
            handle: handle.clone(),
            pending: Mutex::default(),
            ended: AtomicBool::new(false),
        });
        t.by_owner
            .entry(owner)
            .or_default()
            .insert(watch.id, (storage.clone(), watch.clone()));
        t.by_bucket
            .entry(storage)
            .or_default()
            .insert(watch.clone());
        self.active.fetch_add(1, Ordering::Relaxed);
        Ok(watch)
    }

    /// End `owner`'s watch `id`; queued notices for it are discarded.
    /// Unknown ids and other instances' watches are ignored.
    pub(crate) fn remove(&self, owner: Pid, id: u64) {
        let mut t = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((storage, watch)) = t.by_owner.get_mut(&owner).and_then(|w| w.remove(&id)) else {
            return;
        };
        if t.by_owner.get(&owner).is_some_and(BTreeMap::is_empty) {
            t.by_owner.remove(&owner);
        }
        self.drop_watch(&mut t, &storage, &watch);
    }

    /// End every watch `owner` holds.
    pub(crate) fn exited(&self, owner: Pid) {
        let mut t = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        for (storage, watch) in t.by_owner.remove(&owner).unwrap_or_default().into_values() {
            self.drop_watch(&mut t, &storage, &watch);
        }
    }

    fn drop_watch(&self, t: &mut Table, storage: &Key, watch: &Watch) {
        watch.ended.store(true, Ordering::Relaxed);
        self.active.fetch_sub(1, Ordering::Relaxed);
        if let Some(watches) = t.by_bucket.get_mut(storage) {
            watches.remove(watch);
            if watches.is_empty() {
                t.by_bucket.remove(storage);
            }
        }
    }

    /// `changed` keys of the bucket at `storage` now have `revision`.
    pub(crate) fn notify(&self, storage: &Key, changed: &[String], revision: u64) {
        if self.active.load(Ordering::Relaxed) == 0 {
            return;
        }
        // Collect under the lock, deliver outside it.
        let mut due: Vec<(Arc<Watch>, &str)> = Vec::new();
        {
            let t = self.table.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(bucket) = t.by_bucket.get(storage) else {
                return;
            };
            for key in changed {
                if let Some(ws) = bucket.exact.get(key) {
                    due.extend(ws.values().map(|w| (w.clone(), key.as_str())));
                }
                due.extend(
                    bucket
                        .prefix
                        .values()
                        .filter(|w| key.starts_with(&w.key))
                        .map(|w| (w.clone(), key.as_str())),
                );
            }
        }
        for (watch, key) in due {
            watch.note(key, revision);
        }
    }
}

impl Watch {
    /// Tell the watcher `key` is at `revision`: queue a notice, or raise the
    /// one already queued.
    pub(crate) fn note(self: &Arc<Self>, key: &str, revision: u64) {
        let queue = {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            match pending.get_mut(key) {
                Some(r) => {
                    *r = (*r).max(revision);
                    false
                }
                None => {
                    pending.insert(key.to_string(), revision);
                    true
                }
            }
        };
        if queue {
            let notice = Notice {
                watch: self.clone(),
                key: key.to_string(),
            };
            self.handle.signal(Info::Late(LateEvent(Arc::new(notice))));
        }
    }
}

/// A queued notice for one key of one watch.
#[derive(Debug)]
struct Notice {
    watch: Arc<Watch>,
    key: String,
}

impl Late for Notice {
    fn take(&self) -> Option<(String, Envelope)> {
        let revision = self
            .watch
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key)?;
        if self.watch.ended.load(Ordering::Relaxed) {
            return None;
        }
        let body = serde_json::json!({
            "watch": self.watch.id,
            "bucket": self.watch.bucket,
            "key": self.key,
            "revision": revision,
        });
        Some((
            EVENT.to_string(),
            Envelope::new("application/json", body.to_string()),
        ))
    }
}
