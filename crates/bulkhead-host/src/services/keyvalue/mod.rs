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
use wasmtime::Result;
use wasmtime::component::{Linker, Resource};

use super::{Ctx, HostState, InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::instance::InstanceHandle;
use crate::pid::Pid;

use storage::{BucketHandle, KvStore, Op, Outcome, SHARED, ScanQuery, StoreError};

mod bindings {
    // Generated code carries no doc comments.
    #![allow(missing_docs)]

    wasmtime::component::bindgen!({
        path: [
            "../../wit",
            "../../wit/vendor/wasi-logging",
            "../../wit/vendor/wasi-config",
            "../../wit/vendor/wasi-keyvalue",
            "../../wit/groups",
            "../../wit/watch",
            "../../wit/secrets",
            "../../wit/metrics",
            "../../wit/sql",
            "wit",
        ],
        world: "bulkhead:host/keyvalue",
        // Reads are bound synchronously: they never wait, and an async host
        // function costs about a tenth of a read (spike S25).
        imports: {
            "wasi:keyvalue/store.[method]bucket.get": trappable,
            "wasi:keyvalue/store.[method]bucket.exists": trappable,
            "wasi:keyvalue/store.[method]bucket.list-keys": trappable,
            "wasi:keyvalue/batch.get-many": trappable,
            "bulkhead:watch/watch.get": trappable,
            "bulkhead:watch/watch.revision": trappable,
            "bulkhead:watch/watch.scan": trappable,
            default: async | trappable,
        },
        with: {
            "wasi:keyvalue/store.bucket": super::storage::BucketHandle,
        },
    });
}

use bindings::bulkhead::watch::watch;
use bindings::wasi::keyvalue::{atomics, batch, store};

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
    const INTERFACES: &'static [&'static str] = &[
        "wasi:keyvalue/store",
        "wasi:keyvalue/atomics",
        "wasi:keyvalue/batch",
        "bulkhead:watch/watch",
    ];
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

    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> Result<()> {
        store::add_to_linker::<T, Ctx<Self>>(linker, T::service)?;
        atomics::add_to_linker::<T, Ctx<Self>>(linker, T::service)?;
        batch::add_to_linker::<T, Ctx<Self>>(linker, T::service)?;
        watch::add_to_linker::<T, Ctx<Self>>(linker, T::service)
    }

    fn exited(shared: &KvStore, pid: Pid) {
        shared.watches.exited(pid);
    }
}

impl ServiceCtx<'_, Instance> {
    /// Run one storage operation, counting the time as host time.
    async fn run(
        &mut self,
        bucket: &Resource<BucketHandle>,
        op: Op,
    ) -> Result<std::result::Result<Outcome, store::Error>> {
        // Borrowed, not cloned: the table and the service state are
        // separate fields.
        let handle = self.table.get(bucket)?;
        if !handle.writable && !op.read_only() {
            return Ok(Err(store::Error::AccessDenied));
        }
        Ok(self
            .state
            .store
            .run(handle, op)
            .await
            .map_err(|e| store::Error::Other(e.to_string())))
    }

    /// Run a read now, without waiting; for host functions bound
    /// synchronously.
    fn read(
        &mut self,
        bucket: &Resource<BucketHandle>,
        op: Op,
    ) -> Result<std::result::Result<Outcome, store::Error>> {
        let handle = self.table.get(bucket)?;
        Ok(self
            .state
            .store
            .read(handle, op)
            .map_err(|e| store::Error::Other(e.to_string())))
    }

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

fn unexpected(outcome: &Outcome) -> store::Error {
    store::Error::Other(format!("unexpected storage outcome {outcome:?}"))
}

impl store::Host for ServiceCtx<'_, Instance> {
    async fn open(
        &mut self,
        identifier: String,
    ) -> Result<std::result::Result<Resource<BucketHandle>, store::Error>> {
        // Only buckets the manifest lists (and, for shared ones, the grant
        // includes).
        match self.state.resolve(&identifier, &self.tags.app) {
            Ok(handle) => Ok(Ok(self.table.push(handle)?)),
            Err(Refused::AccessDenied) => Ok(Err(store::Error::AccessDenied)),
            Err(Refused::NoSuchStore) => Ok(Err(store::Error::NoSuchStore)),
        }
    }
}

impl store::HostBucket for ServiceCtx<'_, Instance> {
    fn get(
        &mut self,
        bucket: Resource<BucketHandle>,
        key: String,
    ) -> Result<std::result::Result<Option<Vec<u8>>, store::Error>> {
        Ok(self.read(&bucket, Op::Get(key))?.and_then(|o| match o {
            Outcome::Value(v) => Ok(v),
            other => Err(unexpected(&other)),
        }))
    }

    async fn set(
        &mut self,
        bucket: Resource<BucketHandle>,
        key: String,
        value: Vec<u8>,
    ) -> Result<std::result::Result<(), store::Error>> {
        Ok(self.run(&bucket, Op::Set(key, value)).await?.map(|_| ()))
    }

    async fn delete(
        &mut self,
        bucket: Resource<BucketHandle>,
        key: String,
    ) -> Result<std::result::Result<(), store::Error>> {
        Ok(self.run(&bucket, Op::Delete(key)).await?.map(|_| ()))
    }

    fn exists(
        &mut self,
        bucket: Resource<BucketHandle>,
        key: String,
    ) -> Result<std::result::Result<bool, store::Error>> {
        Ok(self.read(&bucket, Op::Exists(key))?.and_then(|o| match o {
            Outcome::Exists(e) => Ok(e),
            other => Err(unexpected(&other)),
        }))
    }

    fn list_keys(
        &mut self,
        bucket: Resource<BucketHandle>,
        cursor: Option<u64>,
    ) -> Result<std::result::Result<store::KeyResponse, store::Error>> {
        Ok(self.read(&bucket, Op::Keys(cursor))?.and_then(|o| match o {
            Outcome::Keys(keys, cursor) => Ok(store::KeyResponse { keys, cursor }),
            other => Err(unexpected(&other)),
        }))
    }

    async fn drop(&mut self, bucket: Resource<BucketHandle>) -> Result<()> {
        self.table.delete(bucket)?;
        Ok(())
    }
}

impl atomics::Host for ServiceCtx<'_, Instance> {
    async fn increment(
        &mut self,
        bucket: Resource<BucketHandle>,
        key: String,
        delta: u64,
    ) -> Result<std::result::Result<u64, store::Error>> {
        Ok(self
            .run(&bucket, Op::Increment(key, delta))
            .await?
            .and_then(|o| match o {
                Outcome::Count(n) => Ok(n),
                other => Err(unexpected(&other)),
            }))
    }
}

impl batch::Host for ServiceCtx<'_, Instance> {
    fn get_many(
        &mut self,
        bucket: Resource<BucketHandle>,
        keys: Vec<String>,
    ) -> Result<std::result::Result<Vec<Option<(String, Vec<u8>)>>, store::Error>> {
        Ok(self
            .read(&bucket, Op::GetMany(keys))?
            .and_then(|o| match o {
                Outcome::Values(v) => Ok(v),
                other => Err(unexpected(&other)),
            }))
    }

    async fn set_many(
        &mut self,
        bucket: Resource<BucketHandle>,
        key_values: Vec<(String, Vec<u8>)>,
    ) -> Result<std::result::Result<(), store::Error>> {
        Ok(self
            .run(&bucket, Op::SetMany(key_values))
            .await?
            .map(|_| ()))
    }

    async fn delete_many(
        &mut self,
        bucket: Resource<BucketHandle>,
        keys: Vec<String>,
    ) -> Result<std::result::Result<(), store::Error>> {
        Ok(self.run(&bucket, Op::DeleteMany(keys)).await?.map(|_| ()))
    }
}

/// The `bulkhead:watch` functions on the host's own and the protocol's
/// types: what a component's import and a native app's requests both run
/// (ROADMAP Z9b).
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

fn to_watch(e: KvError) -> watch::Error {
    match e {
        KvError::AccessDenied => watch::Error::AccessDenied,
        KvError::NoSuchBucket => watch::Error::NoSuchBucket,
        KvError::LimitReached => watch::Error::LimitReached,
        KvError::TooOld(now) => watch::Error::TooOld(now),
        KvError::Conflict(found) => watch::Error::Conflict(found),
        KvError::Unavailable => watch::Error::Unavailable,
        KvError::Other(why) => watch::Error::Other(why),
    }
}

fn watch_entry(e: Option<(Vec<u8>, u64)>) -> Option<watch::Entry> {
    e.map(|(value, revision)| watch::Entry { value, revision })
}

/// A component's `bulkhead:watch` import, on the WIT's types.
impl watch::Host for ServiceCtx<'_, Instance> {
    async fn watch(
        &mut self,
        bucket: String,
        key: String,
        prefix: bool,
        after: Option<u64>,
    ) -> Result<std::result::Result<watch::Started, watch::Error>> {
        Ok(self
            .kv_watch(&bucket, key, prefix, after)
            .await
            .map(|(id, revision)| watch::Started { id, revision })
            .map_err(to_watch))
    }

    async fn unwatch(&mut self, id: u64) -> Result<std::result::Result<(), watch::Error>> {
        self.kv_unwatch(id);
        Ok(Ok(()))
    }

    fn get(
        &mut self,
        bucket: String,
        key: String,
    ) -> Result<std::result::Result<Option<watch::Entry>, watch::Error>> {
        Ok(self
            .kv_entry(&bucket, key)
            .map(watch_entry)
            .map_err(to_watch))
    }

    async fn get_linearizable(
        &mut self,
        bucket: String,
        key: String,
    ) -> Result<std::result::Result<Option<watch::Entry>, watch::Error>> {
        Ok(self
            .kv_entry_linearizable(&bucket, key)
            .await
            .map(watch_entry)
            .map_err(to_watch))
    }

    fn revision(&mut self, bucket: String) -> Result<std::result::Result<u64, watch::Error>> {
        Ok(self.kv_revision(&bucket).map_err(to_watch))
    }

    fn scan(
        &mut self,
        bucket: String,
        query: watch::ScanQuery,
    ) -> Result<std::result::Result<watch::ScanPage, watch::Error>> {
        let query = ScanQuery {
            prefix: query.prefix,
            start: query.start,
            end: query.end,
            reverse: query.reverse,
            limit: query.limit,
            cursor: query.cursor,
        };
        Ok(self
            .kv_scan(&bucket, query)
            .map(|page| watch::ScanPage {
                entries: page
                    .entries
                    .into_iter()
                    .map(|(key, value, revision)| watch::KeyedEntry {
                        key,
                        value: value.0,
                        revision,
                    })
                    .collect(),
                next: page.next,
                revision: page.revision,
            })
            .map_err(to_watch))
    }

    async fn compare_and_swap(
        &mut self,
        bucket: String,
        key: String,
        expected: Option<u64>,
        value: Vec<u8>,
    ) -> Result<std::result::Result<u64, watch::Error>> {
        Ok(self
            .kv_swap(&bucket, key, expected, value)
            .await
            .map_err(to_watch))
    }

    async fn compare_and_delete(
        &mut self,
        bucket: String,
        key: String,
        expected: u64,
    ) -> Result<std::result::Result<u64, watch::Error>> {
        Ok(self
            .kv_swap_delete(&bucket, key, expected)
            .await
            .map_err(to_watch))
    }
}
