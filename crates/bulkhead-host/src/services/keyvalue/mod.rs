//! `wasi:keyvalue` (`store`, `atomics`, `batch`): term storage in host-owned
//! buckets (the private `storage` module has the storage rules), and
//! `bulkhead:watch`: revisioned reads, compare-and-swap and change watches on
//! the same buckets (the private `watches` module has the delivery rules).
//! Both reach a bucket on the same terms.
//!
//! An instance opens only the buckets its manifest lists: its app's own
//! buckets, and shared buckets (node configuration's `[buckets]`) that its
//! grant includes, read-only or read-write. A write to a read-only shared
//! bucket, or `open` without the capability granted, returns
//! `access-denied`; a shared bucket the node does not configure returns
//! `no-such-store`. Every operation runs
//! in the shared store and its time counts as host time, not against the
//! callback deadline. Bulkhead's own host on the vendored
//! `wasi:keyvalue@0.2.0-draft` WIT; wasmtime's crate is a demo (spike S7).

mod native;
pub(crate) mod publish;
pub(crate) mod storage;
mod watches;

/// The host event a watch notice arrives as.
pub(crate) use watches::EVENT as WATCH_EVENT;

use std::collections::BTreeMap;
use std::sync::Arc;

use bulkhead_bundle::manifest::{AccessMode, AppSpec, Bucket, Capability, Durability};
use bulkhead_native::protocol::KvError;

use super::{InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::instance::InstanceHandle;
use crate::pid::Pid;

use storage::{BucketHandle, KvStore, Op, Outcome, SHARED, ScanQuery, StoreError};

/// The keyvalue service.
pub(crate) struct KeyValue;

/// What the manifest asks of keyvalue for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct KeyValueSpec {
    /// The app's own buckets, by name.
    pub buckets: BTreeMap<String, Bucket>,
    /// Shared buckets the instance may open, by name: the manifest's list,
    /// narrowed to the grant with
    /// [`InstanceSpec::with_granted_buckets`](crate::InstanceSpec::with_granted_buckets).
    pub shared: BTreeMap<String, AccessMode>,
}

/// Keyvalue state for one instance.
pub(crate) struct Instance {
    store: Arc<KvStore>,
    buckets: BTreeMap<String, Bucket>,
    shared: BTreeMap<String, AccessMode>,
    granted: bool,
    /// For delivering watch notices.
    me: InstanceHandle,
    /// `limits.watches_max`.
    watches_max: usize,
    /// Buckets a native app opened (ROADMAP T7), by handle: its index.
    opened: Vec<Opened>,
}

/// A bucket a native app opened, resolved once.
struct Opened {
    /// The name the app opened, for a shared bucket the node may stop
    /// configuring; `None` for one of the app's own buckets.
    shared: Option<String>,
    handle: BucketHandle,
}

/// The most buckets one instance opens; past it, an open fails with
/// `limit-reached`. The manifest lists far fewer.
const OPENED_MAX: usize = 4096;

/// Why a bucket name does not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refused {
    /// Not granted, or not in the manifest.
    AccessDenied,
    /// A shared bucket the node does not configure.
    NoSuchStore,
}

impl Instance {
    /// The bucket `name` opens, on the manifest's and the grant's terms.
    fn resolve(&self, name: &str, app: &str) -> std::result::Result<BucketHandle, Refused> {
        if !self.granted {
            return Err(Refused::AccessDenied);
        }
        if let Some(bucket) = self.buckets.get(name) {
            // A replicated app bucket is kept in the replicated namespace
            // as `<app>/<bucket>`, in the log the pair's shared buckets
            // use; shared bucket names cannot hold a `/`.
            let (space, name) = if bucket.durability == Durability::Replicated {
                (
                    crate::replication::SPACE.to_string(),
                    format!("{app}/{name}"),
                )
            } else {
                (app.to_string(), name.to_string())
            };
            return Ok(BucketHandle {
                space,
                name,
                durability: bucket.durability,
                quota: bucket.quota.map(|q| q.bytes()),
                writable: true,
                read_concurrency: bucket.read_concurrency,
            });
        }
        let Some(mode) = self.shared.get(name) else {
            return Err(Refused::AccessDenied);
        };
        let bucket = self.store.shared(name).ok_or(Refused::NoSuchStore)?;
        let space = if bucket.durability == Durability::Replicated {
            crate::replication::SPACE
        } else {
            SHARED
        };
        Ok(BucketHandle {
            space: space.to_string(),
            name: name.to_string(),
            durability: bucket.durability,
            quota: bucket.quota,
            writable: *mode == AccessMode::Rw,
            read_concurrency: bucket.read_concurrency,
        })
    }
}

impl ServiceCtx<'_, Instance> {
    /// The published shared buckets this instance may read, with their
    /// snapshots; `None` if it reads none.
    pub(crate) fn publications(&self) -> Option<publish::Publications> {
        if !self.state.granted {
            return None;
        }
        let buckets: Vec<_> = self
            .state
            .shared
            .keys()
            .filter_map(|name| Some((name.clone(), self.state.store.published(name)?, None)))
            .collect();
        (!buckets.is_empty()).then(|| publish::Publications {
            buckets,
            changed: self.state.store.publishers.generation(),
        })
    }
}

impl Service for KeyValue {
    const CAPABILITY: Capability = Capability::KeyValue;
    type Spec = KeyValueSpec;
    type Shared = KvStore;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> KeyValueSpec {
        let scope = manifest.capabilities.keyvalue.clone().unwrap_or_default();
        KeyValueSpec {
            buckets: scope.buckets,
            shared: scope.shared,
        }
    }

    fn shared(config: &RuntimeConfig) -> Result<KvStore, HostError> {
        KvStore::open(config.data_dir.as_deref(), config.buckets.clone())
            .map_err(HostError::Storage)
    }

    fn instance(shared: &Arc<KvStore>, spec: &KeyValueSpec, info: &InstanceInfo<'_>) -> Instance {
        Instance {
            store: shared.clone(),
            buckets: spec.buckets.clone(),
            shared: spec.shared.clone(),
            granted: info.granted,
            me: info.handle.clone(),
            watches_max: usize::try_from(info.host_objects.watches).unwrap_or(usize::MAX),
            opened: Vec::new(),
        }
    }

    fn exited(shared: &KvStore, pid: Pid) {
        shared.watches.exited(pid);
    }
}

impl ServiceCtx<'_, Instance> {
    /// Run a `bulkhead:watch` read on `bucket` now.
    fn watch_read(&mut self, bucket: &str, op: Op) -> std::result::Result<Outcome, KvError> {
        let handle = self.watch_bucket(bucket, false)?;
        self.state.store.read(&handle, op).map_err(watch_error)
    }

    async fn run_on(
        &mut self,
        handle: &BucketHandle,
        op: Op,
    ) -> std::result::Result<Outcome, StoreError> {
        self.state.store.run(handle, op).await
    }

    /// Resolve `bucket` for a `bulkhead:watch` operation, which writes when
    /// `write` is set.
    fn watch_bucket(
        &self,
        bucket: &str,
        write: bool,
    ) -> std::result::Result<BucketHandle, KvError> {
        let handle = self
            .state
            .resolve(bucket, &self.tags.app)
            .map_err(|r| match r {
                Refused::AccessDenied => KvError::AccessDenied,
                Refused::NoSuchStore => KvError::NoSuchBucket,
            })?;
        if write && !handle.writable {
            return Err(KvError::AccessDenied);
        }
        Ok(handle)
    }

    /// Run a `bulkhead:watch` operation on `bucket`.
    async fn watch_op(&mut self, bucket: &str, op: Op) -> std::result::Result<Outcome, KvError> {
        let handle = self.watch_bucket(bucket, !op.read_only())?;
        self.run_on(&handle, op).await.map_err(watch_error)
    }
}

fn watch_error(e: StoreError) -> KvError {
    match e {
        StoreError::Unavailable(_) => KvError::Unavailable,
        StoreError::Other(why) => KvError::Other(why),
    }
}

fn unexpected_kv(outcome: &Outcome) -> KvError {
    KvError::Other(format!("unexpected storage outcome {outcome:?}"))
}

/// The `bulkhead:watch` functions, on the protocol's types.
impl ServiceCtx<'_, Instance> {
    /// Watch `key` (or every key under it, with `prefix`) in `bucket`:
    /// the watch's id and the bucket's revision it starts from, after
    /// `after` if given.
    pub(crate) async fn kv_watch(
        &mut self,
        bucket: &str,
        key: String,
        prefix: bool,
        after: Option<u64>,
    ) -> std::result::Result<(u64, u64), KvError> {
        let handle = self.watch_bucket(bucket, false)?;
        let storage = (handle.space.clone(), handle.name.clone());
        let state = &*self.state;
        let Ok(w) =
            state
                .store
                .watches
                .add(storage, bucket, &key, prefix, &state.me, state.watches_max)
        else {
            return Err(KvError::LimitReached);
        };
        // Registered first, so a change from here on is noticed either way.
        let op = match after {
            None => Op::Revision(key),
            Some(after) => Op::Since { key, prefix, after },
        };
        let outcome = self.run_on(&handle, op).await;
        let me = self.state.me.pid();
        let revision = match (outcome, after) {
            (Ok(Outcome::Revision { bucket, .. }), None) => bucket,
            (
                Ok(Outcome::Since {
                    revision,
                    deleted,
                    changed,
                }),
                Some(after),
            ) => {
                if after > revision || deleted > after {
                    self.state.store.watches.remove(me, w.id);
                    return Err(KvError::TooOld(revision));
                }
                for (k, r) in changed {
                    w.note(&k, r);
                }
                revision
            }
            (Ok(other), _) => {
                self.state.store.watches.remove(me, w.id);
                return Err(unexpected_kv(&other));
            }
            (Err(e), _) => {
                self.state.store.watches.remove(me, w.id);
                return Err(watch_error(e));
            }
        };
        Ok((w.id, revision))
    }

    /// Stop the watch `id`.
    pub(crate) fn kv_unwatch(&mut self, id: u64) {
        let me = self.state.me.pid();
        self.state.store.watches.remove(me, id);
    }

    /// `key`'s value and revision in `bucket`, if it has one.
    pub(crate) fn kv_entry(
        &mut self,
        bucket: &str,
        key: String,
    ) -> std::result::Result<Option<(Vec<u8>, u64)>, KvError> {
        match self.watch_read(bucket, Op::Entry(key))? {
            Outcome::Entry(e) => Ok(e),
            other => Err(unexpected_kv(&other)),
        }
    }

    /// As [`Self::kv_entry`], after every write acknowledged before it
    /// (a replicated bucket's primary).
    pub(crate) async fn kv_entry_linearizable(
        &mut self,
        bucket: &str,
        key: String,
    ) -> std::result::Result<Option<(Vec<u8>, u64)>, KvError> {
        let handle = self.watch_bucket(bucket, false)?;
        match self
            .state
            .store
            .run_linearizable(&handle, Op::Entry(key))
            .await
            .map_err(watch_error)?
        {
            Outcome::Entry(e) => Ok(e),
            other => Err(unexpected_kv(&other)),
        }
    }

    /// `bucket`'s revision.
    pub(crate) fn kv_revision(&mut self, bucket: &str) -> std::result::Result<u64, KvError> {
        match self.watch_read(bucket, Op::Revision(String::new()))? {
            Outcome::Revision { bucket, .. } => Ok(bucket),
            other => Err(unexpected_kv(&other)),
        }
    }

    /// A page of `bucket`'s entries matching `query`.
    pub(crate) fn kv_scan(
        &mut self,
        bucket: &str,
        query: ScanQuery,
    ) -> std::result::Result<bulkhead_native::protocol::ScanPage, KvError> {
        match self.watch_read(bucket, Op::Scan(query))? {
            Outcome::Page {
                entries,
                next,
                revision,
            } => Ok(bulkhead_native::protocol::ScanPage {
                entries: entries
                    .into_iter()
                    .map(|(k, v, r)| (k, bulkhead_native::protocol::Bytes(v), r))
                    .collect(),
                next,
                revision,
            }),
            other => Err(unexpected_kv(&other)),
        }
    }

    /// Set `key` to `value` if its revision is `expected` (absent for
    /// `None`): the new revision.
    pub(crate) async fn kv_swap(
        &mut self,
        bucket: &str,
        key: String,
        expected: Option<u64>,
        value: Vec<u8>,
    ) -> std::result::Result<u64, KvError> {
        let op = Op::Swap {
            key,
            expected,
            value,
        };
        swapped(self.watch_op(bucket, op).await)
    }

    /// Delete `key` if its revision is `expected`: the bucket's revision.
    pub(crate) async fn kv_swap_delete(
        &mut self,
        bucket: &str,
        key: String,
        expected: u64,
    ) -> std::result::Result<u64, KvError> {
        let op = Op::SwapDelete { key, expected };
        swapped(self.watch_op(bucket, op).await)
    }
}

fn swapped(outcome: std::result::Result<Outcome, KvError>) -> std::result::Result<u64, KvError> {
    match outcome? {
        Outcome::Swapped(revision) => Ok(revision),
        Outcome::Conflict(found) => Err(KvError::Conflict(found)),
        other => Err(unexpected_kv(&other)),
    }
}
