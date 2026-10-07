//! `wasi:keyvalue` (`store`, `atomics`, `batch`): term storage in host-owned
//! buckets (the private `storage` module has the storage rules).
//!
//! An instance opens only the buckets its manifest lists. Without the
//! capability granted, `open` returns `access-denied`. Every operation runs
//! in the shared store and its time counts as host time, not against the
//! callback deadline. Paddock's own host on the vendored
//! `wasi:keyvalue@0.2.0-draft` WIT; wasmtime's crate is a demo (spike S7).

mod storage;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use paddock_bundle::manifest::{AppSpec, Bucket, Capability};
use wasmtime::Result;
use wasmtime::component::{Linker, Resource};

use super::{Ctx, HostState, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;

use storage::{BucketHandle, KvStore, Op, Outcome};

mod bindings {
    // Generated code carries no doc comments.
    #![allow(missing_docs)]

    wasmtime::component::bindgen!({
        path: [
            "../../wit",
            "../../wit/vendor/wasi-logging",
            "../../wit/vendor/wasi-config",
            "../../wit/vendor/wasi-keyvalue",
            "wit",
        ],
        world: "paddock:host/keyvalue",
        imports: { default: async | trappable },
        with: {
            "wasi:keyvalue/store.bucket": super::storage::BucketHandle,
        },
    });
}

use bindings::wasi::keyvalue::{atomics, batch, store};

/// The keyvalue service.
pub(crate) struct KeyValue;

/// What the manifest asks of keyvalue for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct KeyValueSpec {
    /// Buckets the instance may open, by name.
    pub buckets: BTreeMap<String, Bucket>,
}

/// Keyvalue state for one instance.
pub(crate) struct Instance {
    store: Arc<KvStore>,
    buckets: BTreeMap<String, Bucket>,
    granted: bool,
}

impl Service for KeyValue {
    const CAPABILITY: Capability = Capability::KeyValue;
    const INTERFACES: &'static [&'static str] = &[
        "wasi:keyvalue/store",
        "wasi:keyvalue/atomics",
        "wasi:keyvalue/batch",
    ];
    type Spec = KeyValueSpec;
    type Shared = KvStore;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> KeyValueSpec {
        KeyValueSpec {
            buckets: manifest
                .capabilities
                .keyvalue
                .as_ref()
                .map(|kv| kv.buckets.clone())
                .unwrap_or_default(),
        }
    }

    fn shared(config: &RuntimeConfig) -> Result<KvStore, HostError> {
        KvStore::open(config.data_dir.as_deref()).map_err(HostError::Storage)
    }

    fn instance(shared: &Arc<KvStore>, spec: &KeyValueSpec, granted: bool) -> Instance {
        Instance {
            store: shared.clone(),
            buckets: spec.buckets.clone(),
            granted,
        }
    }

    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> Result<()> {
        store::add_to_linker::<T, Ctx<Self>>(linker, T::service)?;
        atomics::add_to_linker::<T, Ctx<Self>>(linker, T::service)?;
        batch::add_to_linker::<T, Ctx<Self>>(linker, T::service)
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
        let started = Instant::now();
        let outcome = self.state.store.run(&handle, op).await;
        self.add_host_time(started.elapsed());
        Ok(outcome.map_err(store::Error::Other))
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
        if !self.state.granted {
            return Ok(Err(store::Error::AccessDenied));
        }
        // Only buckets the manifest lists.
        let Some(bucket) = self.state.buckets.get(&identifier) else {
            return Ok(Err(store::Error::AccessDenied));
        };
        let handle = BucketHandle {
            app: self.tags.app.clone(),
            name: identifier,
            durability: bucket.durability,
            quota: bucket.quota.map(|q| q.bytes()),
        };
        Ok(Ok(self.table.push(handle)?))
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
