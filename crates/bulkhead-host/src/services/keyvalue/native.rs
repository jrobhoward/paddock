//! Keyvalue for native-process apps: each [`KvRequest`] from the app's
//! protocol, answered with the code a component's `wasi:keyvalue` and
//! `bulkhead:watch` imports run, on the same terms (the manifest's
//! buckets, the grant, read-only shared buckets, quotas, watch limits).
//! A watch notice is delivered with the key's value and revision, read
//! when the notice is taken, so a native app keeping a copy needs no
//! request per change (spikes S28 and S31).

use bulkhead_native::protocol::{self as p, Bytes, KvAnswer, KvError, KvRequest};
use serde::Deserialize;

use super::bindings::bulkhead::watch::watch;
use super::storage::{Op, Outcome, StoreError};
use super::{Instance, watch_error};
use crate::message::Envelope;
use crate::services::ServiceCtx;

fn from_watch(e: watch::Error) -> KvError {
    match e {
        watch::Error::AccessDenied => KvError::AccessDenied,
        watch::Error::NoSuchBucket => KvError::NoSuchBucket,
        watch::Error::LimitReached => KvError::LimitReached,
        watch::Error::TooOld(now) => KvError::TooOld(now),
        watch::Error::Conflict(found) => KvError::Conflict(found),
        watch::Error::Unavailable => KvError::Unavailable,
        watch::Error::Other(why) => KvError::Other(why),
    }
}

fn unexpected(outcome: &Outcome) -> KvError {
    KvError::Other(format!("unexpected storage outcome {outcome:?}"))
}

/// A host function's own result: its trap (a resource-table failure,
/// never expected here) becomes an error value.
fn flat<T>(r: wasmtime::Result<Result<T, watch::Error>>) -> Result<T, KvError> {
    match r {
        Ok(inner) => inner.map_err(from_watch),
        Err(e) => Err(KvError::Other(e.to_string())),
    }
}

fn entry(e: Option<watch::Entry>) -> KvAnswer {
    KvAnswer::Entry(e.map(|e| (Bytes(e.value), e.revision)))
}

impl ServiceCtx<'_, Instance> {
    /// Run a store operation on `bucket` (resolved and checked as a watch
    /// operation is: a write needs a writable bucket).
    async fn native_op(&mut self, bucket: &str, op: Op) -> Result<Outcome, KvError> {
        let handle = self
            .watch_bucket(bucket, !op.read_only())
            .map_err(from_watch)?;
        if op.read_only() {
            self.state.store.read(&handle, op)
        } else {
            self.state.store.run(&handle, op).await
        }
        .map_err(|e: StoreError| from_watch(watch_error(e)))
    }

    /// Answer a native app's keyvalue request.
    pub(crate) async fn native(&mut self, request: KvRequest) -> Result<KvAnswer, KvError> {
        use watch::Host;
        Ok(match request {
            KvRequest::Get { bucket, key } => match self.native_op(&bucket, Op::Get(key)).await? {
                Outcome::Value(v) => KvAnswer::Value(v.map(Bytes)),
                other => return Err(unexpected(&other)),
            },
            KvRequest::Set { bucket, key, value } => {
                self.native_op(&bucket, Op::Set(key, value.0)).await?;
                KvAnswer::Unit
            }
            KvRequest::Delete { bucket, key } => {
                self.native_op(&bucket, Op::Delete(key)).await?;
                KvAnswer::Unit
            }
            KvRequest::Exists { bucket, key } => {
                match self.native_op(&bucket, Op::Exists(key)).await? {
                    Outcome::Exists(e) => KvAnswer::Exists(e),
                    other => return Err(unexpected(&other)),
                }
            }
            KvRequest::Keys { bucket, cursor } => {
                match self.native_op(&bucket, Op::Keys(cursor)).await? {
                    Outcome::Keys(keys, next) => KvAnswer::Keys(keys, next),
                    other => return Err(unexpected(&other)),
                }
            }
            KvRequest::Increment { bucket, key, delta } => {
                match self.native_op(&bucket, Op::Increment(key, delta)).await? {
                    Outcome::Count(n) => KvAnswer::Count(n),
                    other => return Err(unexpected(&other)),
                }
            }
            KvRequest::GetMany { bucket, keys } => {
                match self.native_op(&bucket, Op::GetMany(keys)).await? {
                    Outcome::Values(found) => KvAnswer::Values(
                        found
                            .into_iter()
                            .map(|f| f.map(|(_, v)| Bytes(v)))
                            .collect(),
                    ),
                    other => return Err(unexpected(&other)),
                }
            }
            KvRequest::SetMany { bucket, entries } => {
                let entries = entries.into_iter().map(|(k, v)| (k, v.0)).collect();
                self.native_op(&bucket, Op::SetMany(entries)).await?;
                KvAnswer::Unit
            }
            KvRequest::DeleteMany { bucket, keys } => {
                self.native_op(&bucket, Op::DeleteMany(keys)).await?;
                KvAnswer::Unit
            }
            KvRequest::Entry {
                bucket,
                key,
                linearizable: false,
            } => entry(flat(Host::get(self, bucket, key))?),
            KvRequest::Entry {
                bucket,
                key,
                linearizable: true,
            } => entry(flat(Host::get_linearizable(self, bucket, key).await)?),
            KvRequest::Revision { bucket } => {
                KvAnswer::Revision(flat(Host::revision(self, bucket))?)
            }
            KvRequest::Scan { bucket, query } => {
                let query = watch::ScanQuery {
                    prefix: query.prefix,
                    start: query.start,
                    end: query.end,
                    reverse: query.reverse,
                    limit: query.limit,
                    cursor: query.cursor,
                };
                let page = flat(Host::scan(self, bucket, query))?;
                KvAnswer::Page(p::ScanPage {
                    entries: page
                        .entries
                        .into_iter()
                        .map(|e| (e.key, Bytes(e.value), e.revision))
                        .collect(),
                    next: page.next,
                    revision: page.revision,
                })
            }
            KvRequest::CompareAndSwap {
                bucket,
                key,
                expected,
                value,
            } => KvAnswer::Revision(flat(
                Host::compare_and_swap(self, bucket, key, expected, value.0).await,
            )?),
            KvRequest::CompareAndDelete {
                bucket,
                key,
                expected,
            } => KvAnswer::Revision(flat(
                Host::compare_and_delete(self, bucket, key, expected).await,
            )?),
            KvRequest::Watch {
                bucket,
                key,
                prefix,
                after,
            } => {
                let started = flat(Host::watch(self, bucket, key, prefix, after).await)?;
                KvAnswer::Started {
                    id: started.id,
                    revision: started.revision,
                }
            }
            KvRequest::Unwatch(id) => {
                flat(Host::unwatch(self, id).await)?;
                KvAnswer::Unit
            }
        })
    }

    /// The native form of a `kv-changed` notice: the key's value and
    /// revision read now. `None` if `msg` is not one, or its bucket no
    /// longer reads.
    pub(crate) fn native_notice(&mut self, msg: &Envelope) -> Option<p::Info> {
        #[derive(Deserialize)]
        struct Notice {
            watch: u64,
            bucket: String,
            key: String,
            revision: u64,
        }
        let notice: Notice = serde_json::from_slice(&msg.body).ok()?;
        // A bucket that no longer opens (an operator change) leaves the
        // notice a plain `kv-changed` event, not one that looks like a
        // delete.
        let entry = match self
            .watch_read(&notice.bucket, Op::Entry(notice.key.clone()))
            .ok()?
        {
            Outcome::Entry(e) => e.map(|(v, r)| (Bytes(v), r)),
            _ => return None,
        };
        Some(p::Info::KvChanged {
            watch: notice.watch,
            bucket: notice.bucket,
            key: notice.key,
            entry,
            revision: notice.revision,
        })
    }
}
