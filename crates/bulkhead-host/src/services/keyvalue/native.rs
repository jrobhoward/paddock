//! Keyvalue for native-process apps: each [`KvRequest`] from the app's
//! protocol, answered with the functions a component's `bulkhead:keyvalue`
//! and `bulkhead:watch` imports run too (ROADMAP Z9b), on the same terms (the manifest's
//! buckets, the grant, read-only shared buckets, quotas, watch limits).
//! A watch notice is delivered with the key's value and revision, read
//! when the notice is taken, so a native app keeping a copy needs no
//! request per change (spikes S28 and S31).

use bulkhead_native::hot::KvOp;
use bulkhead_native::protocol::{self as p, Bytes, KvAnswer, KvError, KvRequest};
use serde::Deserialize;

use super::storage::{Op, Outcome, ScanQuery};
use super::{Instance, watch_error};
use crate::message::Envelope;
use crate::services::ServiceCtx;

fn unexpected(outcome: &Outcome) -> KvError {
    KvError::Other(format!("unexpected storage outcome {outcome:?}"))
}

fn entry(e: Option<(Vec<u8>, u64)>) -> KvAnswer {
    KvAnswer::Entry(e.map(|(value, revision)| (Bytes(value), revision)))
}

impl ServiceCtx<'_, Instance> {
    /// Run a store operation on `bucket` (resolved and checked as a watch
    /// operation is: a write needs a writable bucket).
    async fn native_op(&mut self, bucket: &str, op: Op) -> Result<Outcome, KvError> {
        let handle = self.watch_bucket(bucket, !op.read_only())?;
        if op.read_only() {
            self.state.store.read(&handle, op)
        } else {
            self.state.store.run(&handle, op).await
        }
        .map_err(watch_error)
    }

    /// Open `bucket` for hot frames: its handle, the index of its entry.
    /// Checked as an operation naming it is, for reading (a write through
    /// the handle checks `writable` then).
    pub(crate) fn native_open(&mut self, bucket: &str) -> Result<u32, KvError> {
        let handle = self.watch_bucket(bucket, false)?;
        let opened = &mut self.state.opened;
        if let Some(i) = opened.iter().position(|o| o.handle == handle) {
            return u32::try_from(i).map_err(|_| KvError::LimitReached);
        }
        if opened.len() >= super::OPENED_MAX {
            return Err(KvError::LimitReached);
        }
        let shared = (!self.state.buckets.contains_key(bucket)).then(|| bucket.to_string());
        opened.push(super::Opened { shared, handle });
        u32::try_from(opened.len() - 1).map_err(|_| KvError::LimitReached)
    }

    /// Answer a hot frame's keyvalue request, on a bucket opened with
    /// [`Self::native_open`]. A handle this instance was not given is
    /// `access-denied`; a shared bucket the node no longer configures is
    /// `no-such-bucket`, as a request naming it would be.
    pub(crate) async fn native_hot(
        &mut self,
        bucket: u32,
        op: KvOp,
        key: String,
        value: Vec<u8>,
    ) -> Result<Outcome, KvError> {
        let opened = usize::try_from(bucket)
            .ok()
            .and_then(|i| self.state.opened.get(i))
            .ok_or(KvError::AccessDenied)?;
        if let Some(name) = &opened.shared
            && self.state.store.shared(name).is_none()
        {
            return Err(KvError::NoSuchBucket);
        }
        let op = match op {
            KvOp::Get => Op::Get(key),
            KvOp::Set => Op::Set(key, value),
            KvOp::Delete => Op::Delete(key),
            KvOp::Exists => Op::Exists(key),
        };
        if !op.read_only() && !opened.handle.writable {
            return Err(KvError::AccessDenied);
        }
        let handle = &opened.handle;
        if op.read_only() {
            self.state.store.read(handle, op)
        } else {
            self.state.store.run(handle, op).await
        }
        .map_err(watch_error)
    }

    /// Answer a native app's keyvalue request.
    pub(crate) async fn native(&mut self, request: KvRequest) -> Result<KvAnswer, KvError> {
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
            } => entry(self.kv_entry(&bucket, key)?),
            KvRequest::Entry {
                bucket,
                key,
                linearizable: true,
            } => entry(self.kv_entry_linearizable(&bucket, key).await?),
            KvRequest::Revision { bucket } => KvAnswer::Revision(self.kv_revision(&bucket)?),
            KvRequest::Scan { bucket, query } => {
                let query = ScanQuery {
                    prefix: query.prefix,
                    start: query.start,
                    end: query.end,
                    reverse: query.reverse,
                    limit: query.limit,
                    cursor: query.cursor,
                };
                KvAnswer::Page(self.kv_scan(&bucket, query)?)
            }
            KvRequest::CompareAndSwap {
                bucket,
                key,
                expected,
                value,
            } => KvAnswer::Revision(self.kv_swap(&bucket, key, expected, value.0).await?),
            KvRequest::CompareAndDelete {
                bucket,
                key,
                expected,
            } => KvAnswer::Revision(self.kv_swap_delete(&bucket, key, expected).await?),
            KvRequest::Watch {
                bucket,
                key,
                prefix,
                after,
            } => {
                let (id, revision) = self.kv_watch(&bucket, key, prefix, after).await?;
                KvAnswer::Started { id, revision }
            }
            KvRequest::Unwatch(id) => {
                self.kv_unwatch(id);
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
