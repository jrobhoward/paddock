//! `wasi:keyvalue` (`store`, `atomics`, `batch`): term storage in host-owned
//! buckets (the private `storage` module has the storage rules), and
//! `paddock:watch`: revisioned reads, compare-and-swap and change watches on
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
//! callback deadline. Paddock's own host on the vendored
//! `wasi:keyvalue@0.2.0-draft` WIT; wasmtime's crate is a demo (spike S7).

mod storage;
mod watches;

use std::collections::BTreeMap;
use std::sync::Arc;

use paddock_bundle::manifest::{AccessMode, AppSpec, Bucket, Capability};
use wasmtime::Result;
use wasmtime::component::{Linker, Resource};

use super::{Ctx, HostState, InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::instance::InstanceHandle;
use crate::pid::Pid;

use storage::{BucketHandle, KvStore, Op, Outcome, SHARED};

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
        world: "paddock:host/keyvalue",
        imports: { default: async | trappable },
        with: {
            "wasi:keyvalue/store.bucket": super::storage::BucketHandle,
        },
    });
}

use bindings::paddock::watch::watch;
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
}

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
            return Ok(BucketHandle {
                space: app.to_string(),
                name: name.to_string(),
                durability: bucket.durability,
                quota: bucket.quota.map(|q| q.bytes()),
                writable: true,
            });
        }
        let Some(mode) = self.shared.get(name) else {
            return Err(Refused::AccessDenied);
        };
        let bucket = self.store.shared(name).ok_or(Refused::NoSuchStore)?;
        Ok(BucketHandle {
            space: SHARED.to_string(),
            name: name.to_string(),
            durability: bucket.durability,
            quota: bucket.quota,
            writable: *mode == AccessMode::Rw,
        })
    }
}

impl Service for KeyValue {
    const CAPABILITY: Capability = Capability::KeyValue;
    const INTERFACES: &'static [&'static str] = &[
        "wasi:keyvalue/store",
        "wasi:keyvalue/atomics",
        "wasi:keyvalue/batch",
        "paddock:watch/watch",
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
        let handle = self.table.get(bucket)?.clone();
        if !handle.writable && !op.read_only() {
            return Ok(Err(store::Error::AccessDenied));
        }
        Ok(self.run_on(&handle, op).await.map_err(store::Error::Other))
    }

    async fn run_on(
        &mut self,
        handle: &BucketHandle,
        op: Op,
    ) -> std::result::Result<Outcome, String> {
        self.state.store.run(handle, op).await
    }

    /// Resolve `bucket` for a `paddock:watch` operation, which writes when
    /// `write` is set.
    fn watch_bucket(
        &self,
        bucket: &str,
        write: bool,
    ) -> std::result::Result<BucketHandle, watch::Error> {
        let handle = self
            .state
            .resolve(bucket, &self.tags.app)
            .map_err(|r| match r {
                Refused::AccessDenied => watch::Error::AccessDenied,
                Refused::NoSuchStore => watch::Error::NoSuchBucket,
            })?;
        if write && !handle.writable {
            return Err(watch::Error::AccessDenied);
        }
        Ok(handle)
    }

    /// Run a `paddock:watch` operation on `bucket`.
    async fn watch_op(
        &mut self,
        bucket: &str,
        op: Op,
    ) -> std::result::Result<Outcome, watch::Error> {
        let handle = self.watch_bucket(bucket, !op.read_only())?;
        self.run_on(&handle, op).await.map_err(watch::Error::Other)
    }
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
    async fn get(
        &mut self,
        bucket: Resource<BucketHandle>,
        key: String,
    ) -> Result<std::result::Result<Option<Vec<u8>>, store::Error>> {
        Ok(self
            .run(&bucket, Op::Get(key))
            .await?
            .and_then(|o| match o {
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

    async fn exists(
        &mut self,
        bucket: Resource<BucketHandle>,
        key: String,
    ) -> Result<std::result::Result<bool, store::Error>> {
        Ok(self
            .run(&bucket, Op::Exists(key))
            .await?
            .and_then(|o| match o {
                Outcome::Exists(e) => Ok(e),
                other => Err(unexpected(&other)),
            }))
    }

    async fn list_keys(
        &mut self,
        bucket: Resource<BucketHandle>,
        cursor: Option<u64>,
    ) -> Result<std::result::Result<store::KeyResponse, store::Error>> {
        Ok(self
            .run(&bucket, Op::Keys(cursor))
            .await?
            .and_then(|o| match o {
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
    async fn get_many(
        &mut self,
        bucket: Resource<BucketHandle>,
        keys: Vec<String>,
    ) -> Result<std::result::Result<Vec<Option<(String, Vec<u8>)>>, store::Error>> {
        Ok(self
            .run(&bucket, Op::GetMany(keys))
            .await?
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

impl watch::Host for ServiceCtx<'_, Instance> {
    async fn watch(
        &mut self,
        bucket: String,
        key: String,
        prefix: bool,
        after: Option<u64>,
    ) -> Result<std::result::Result<watch::Started, watch::Error>> {
        let handle = match self.watch_bucket(&bucket, false) {
            Ok(h) => h,
            Err(e) => return Ok(Err(e)),
        };
        let storage = (handle.space.clone(), handle.name.clone());
        let state = &*self.state;
        let Ok(w) =
            state
                .store
                .watches
                .add(storage, &bucket, &key, prefix, &state.me, state.watches_max)
        else {
            return Ok(Err(watch::Error::LimitReached));
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
                    return Ok(Err(watch::Error::TooOld(revision)));
                }
                for (k, r) in changed {
                    w.note(&k, r);
                }
                revision
            }
            (Ok(other), _) => {
                self.state.store.watches.remove(me, w.id);
                return Ok(Err(watch::Error::Other(format!(
                    "unexpected storage outcome {other:?}"
                ))));
            }
            (Err(e), _) => {
                self.state.store.watches.remove(me, w.id);
                return Ok(Err(watch::Error::Other(e)));
            }
        };
        Ok(Ok(watch::Started { id: w.id, revision }))
    }

    async fn unwatch(&mut self, id: u64) -> Result<std::result::Result<(), watch::Error>> {
        let me = self.state.me.pid();
        self.state.store.watches.remove(me, id);
        Ok(Ok(()))
    }

    async fn get(
        &mut self,
        bucket: String,
        key: String,
    ) -> Result<std::result::Result<Option<watch::Entry>, watch::Error>> {
        Ok(self
            .watch_op(&bucket, Op::Entry(key))
            .await
            .and_then(|o| match o {
                Outcome::Entry(e) => {
                    Ok(e.map(|(value, revision)| watch::Entry { value, revision }))
                }
                other => Err(watch::Error::Other(format!(
                    "unexpected storage outcome {other:?}"
                ))),
            }))
    }

    async fn revision(&mut self, bucket: String) -> Result<std::result::Result<u64, watch::Error>> {
        Ok(self
            .watch_op(&bucket, Op::Revision(String::new()))
            .await
            .and_then(|o| match o {
                Outcome::Revision { bucket, .. } => Ok(bucket),
                other => Err(watch::Error::Other(format!(
                    "unexpected storage outcome {other:?}"
                ))),
            }))
    }

    async fn compare_and_swap(
        &mut self,
        bucket: String,
        key: String,
        expected: Option<u64>,
        value: Vec<u8>,
    ) -> Result<std::result::Result<u64, watch::Error>> {
        let op = Op::Swap {
            key,
            expected,
            value,
        };
        Ok(swapped(self.watch_op(&bucket, op).await))
    }

    async fn compare_and_delete(
        &mut self,
        bucket: String,
        key: String,
        expected: u64,
    ) -> Result<std::result::Result<u64, watch::Error>> {
        let op = Op::SwapDelete { key, expected };
        Ok(swapped(self.watch_op(&bucket, op).await))
    }
}

fn swapped(
    outcome: std::result::Result<Outcome, watch::Error>,
) -> std::result::Result<u64, watch::Error> {
    match outcome? {
        Outcome::Swapped(revision) => Ok(revision),
        Outcome::Conflict(found) => Err(watch::Error::Conflict(found)),
        other => Err(watch::Error::Other(format!(
            "unexpected storage outcome {other:?}"
        ))),
    }
}
