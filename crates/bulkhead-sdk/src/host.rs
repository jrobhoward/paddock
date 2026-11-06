//! The host functions the SDK's modules call. In a component they are
//! wit-bindgen's imports; in a native program (Unix, not wasm32) they are
//! the same functions over `bulkhead-native`'s protocol, taking and
//! returning the same generated types, so the modules above them read the
//! same either way.

// Each service only with its feature, as its module above it.
#[cfg(all(feature = "groups", not(all(unix, not(target_arch = "wasm32")))))]
pub(crate) use crate::bindings::bulkhead::groups::groups;
#[cfg(all(feature = "metrics", not(all(unix, not(target_arch = "wasm32")))))]
pub(crate) use crate::bindings::bulkhead::metrics::metrics;
#[cfg(all(feature = "secrets", not(all(unix, not(target_arch = "wasm32")))))]
pub(crate) use crate::bindings::bulkhead::secrets::store as secrets_store;
#[cfg(all(feature = "watch", not(all(unix, not(target_arch = "wasm32")))))]
pub(crate) use crate::bindings::bulkhead::watch::watch;
#[cfg(all(feature = "config", not(all(unix, not(target_arch = "wasm32")))))]
pub(crate) use crate::bindings::wasi::config::store as config_store;
#[cfg(all(feature = "keyvalue", not(all(unix, not(target_arch = "wasm32")))))]
pub(crate) use crate::bindings::wasi::keyvalue;
#[cfg(not(all(unix, not(target_arch = "wasm32"))))]
pub(crate) use crate::bindings::{bulkhead::runtime::process, wasi::logging::logging};

#[cfg(all(feature = "config", unix, not(target_arch = "wasm32")))]
pub(crate) use native::config_store;
#[cfg(all(feature = "groups", unix, not(target_arch = "wasm32")))]
pub(crate) use native::groups;
#[cfg(all(feature = "keyvalue", unix, not(target_arch = "wasm32")))]
pub(crate) use native::keyvalue;
#[cfg(all(feature = "metrics", unix, not(target_arch = "wasm32")))]
pub(crate) use native::metrics;
#[cfg(all(feature = "secrets", unix, not(target_arch = "wasm32")))]
pub(crate) use native::secrets_store;
#[cfg(all(feature = "watch", unix, not(target_arch = "wasm32")))]
pub(crate) use native::watch;
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub(crate) use native::{logging, process};

/// The native halves, and the conversions between the protocol's types
/// and the generated ones.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub(crate) mod native {
    use bulkhead_native::app::Ctx;
    use bulkhead_native::protocol as p;

    use crate::bindings::bulkhead::runtime::types as wit;

    pub(crate) fn pid(pid: wit::Pid) -> p::Pid {
        p::Pid {
            node: pid.node,
            id: pid.id,
            incarnation: pid.incarnation,
        }
    }

    pub(crate) fn wit_pid(pid: p::Pid) -> wit::Pid {
        wit::Pid {
            node: pid.node,
            id: pid.id,
            incarnation: pid.incarnation,
        }
    }

    pub(crate) fn envelope(env: &wit::Envelope) -> p::Envelope {
        p::Envelope {
            content_type: env.content_type.clone(),
            schema: env.schema.clone(),
            body: env.body.clone(),
        }
    }

    pub(crate) fn wit_envelope(env: p::Envelope) -> wit::Envelope {
        wit::Envelope {
            content_type: env.content_type,
            schema: env.schema,
            body: env.body,
        }
    }

    pub(crate) fn exit_reason(reason: &wit::ExitReason) -> p::ExitReason {
        match reason {
            wit::ExitReason::Normal => p::ExitReason::Normal,
            wit::ExitReason::Shutdown => p::ExitReason::Shutdown,
            wit::ExitReason::Killed => p::ExitReason::Killed,
            wit::ExitReason::Noproc => p::ExitReason::Noproc,
            wit::ExitReason::Noconnection => p::ExitReason::Noconnection,
            wit::ExitReason::Timeout => p::ExitReason::Timeout,
            wit::ExitReason::Crashed(why) => p::ExitReason::Crashed(why.clone()),
        }
    }

    pub(crate) fn wit_exit_reason(reason: p::ExitReason) -> wit::ExitReason {
        match reason {
            p::ExitReason::Normal => wit::ExitReason::Normal,
            p::ExitReason::Shutdown => wit::ExitReason::Shutdown,
            p::ExitReason::Killed => wit::ExitReason::Killed,
            p::ExitReason::Noproc => wit::ExitReason::Noproc,
            p::ExitReason::Noconnection => wit::ExitReason::Noconnection,
            p::ExitReason::Timeout => wit::ExitReason::Timeout,
            p::ExitReason::Crashed(why) => wit::ExitReason::Crashed(why),
        }
    }

    fn wit_call_error(e: p::CallError) -> wit::CallError {
        match e {
            p::CallError::Noproc => wit::CallError::Noproc,
            p::CallError::Noconnection => wit::CallError::Noconnection,
            p::CallError::Timeout => wit::CallError::Timeout,
            p::CallError::Overloaded => wit::CallError::Overloaded,
            p::CallError::TooLarge => wit::CallError::TooLarge,
            p::CallError::AccessDenied => wit::CallError::AccessDenied,
            p::CallError::Exited(reason) => wit::CallError::Exited(wit_exit_reason(reason)),
        }
    }

    /// `bulkhead:runtime/process`, over the protocol.
    pub(crate) mod process {
        use super::{
            Ctx, envelope, exit_reason, p, pid, wit, wit_call_error, wit_envelope, wit_pid,
        };

        pub(crate) fn self_() -> wit::Pid {
            wit_pid(Ctx::current().self_pid())
        }

        pub(crate) fn cast(to: wit::Pid, msg: &wit::Envelope) {
            Ctx::current().cast(pid(to), envelope(msg));
        }

        /// No WIT counterpart: native programs only (ROADMAP I1).
        pub(crate) fn try_cast(to: wit::Pid, msg: &wit::Envelope) -> Result<(), p::CastError> {
            Ctx::current().try_cast(pid(to), envelope(msg))
        }

        /// No WIT counterpart: native programs only (ROADMAP I2).
        pub(crate) fn cast_timeout(
            to: wit::Pid,
            msg: &wit::Envelope,
            timeout_ms: u32,
        ) -> Result<(), p::CastError> {
            Ctx::current().cast_timeout(pid(to), envelope(msg), timeout_ms)
        }

        pub(crate) fn call(
            to: wit::Pid,
            msg: &wit::Envelope,
            timeout_ms: u32,
        ) -> Result<wit::Envelope, wit::CallError> {
            Ctx::current()
                .call(pid(to), envelope(msg), timeout_ms)
                .map(wit_envelope)
                .map_err(wit_call_error)
        }

        pub(crate) fn reply(call: u64, msg: &wit::Envelope) {
            Ctx::current().reply(call, envelope(msg));
        }

        pub(crate) fn link(other: wit::Pid) {
            Ctx::current().link(pid(other));
        }

        pub(crate) fn unlink(other: wit::Pid) {
            Ctx::current().unlink(pid(other));
        }

        pub(crate) fn trap_exits(on: bool) {
            Ctx::current().trap_exits(on);
        }

        pub(crate) fn monitor(other: wit::Pid) -> u64 {
            Ctx::current().monitor(pid(other))
        }

        pub(crate) fn demonitor(monitor: u64) {
            Ctx::current().demonitor(monitor);
        }

        pub(crate) fn register(name: &str) -> Result<(), wit::RegisterError> {
            Ctx::current().register(name).map_err(|e| match e {
                p::RegisterError::AccessDenied => wit::RegisterError::AccessDenied,
                p::RegisterError::Taken(holder) => wit::RegisterError::Taken(wit_pid(holder)),
            })
        }

        pub(crate) fn whereis(name: &str) -> Option<wit::Pid> {
            Ctx::current().whereis(name).map(wit_pid)
        }

        pub(crate) fn send_after(ms: u32, msg: &wit::Envelope) -> u64 {
            Ctx::current().send_after(ms, envelope(msg))
        }

        pub(crate) fn cancel_timer(timer: u64) {
            Ctx::current().cancel_timer(timer);
        }

        pub(crate) fn exit(other: wit::Pid, reason: &wit::ExitReason) {
            Ctx::current().exit(pid(other), exit_reason(reason));
        }
    }

    /// `wasi:logging`, over the protocol (any thread).
    pub(crate) mod logging {
        pub(crate) use crate::bindings::wasi::logging::logging::Level;

        /// `context` is not sent: the protocol's log record has the
        /// message alone, as the node records a component's.
        pub(crate) fn log(level: Level, _context: &str, message: &str) {
            use bulkhead_native::protocol::Level as L;
            let level = match level {
                Level::Trace => L::Trace,
                Level::Debug => L::Debug,
                Level::Info => L::Info,
                Level::Warn => L::Warn,
                Level::Error => L::Error,
                Level::Critical => L::Critical,
            };
            bulkhead_native::app::log(level, message);
        }
    }

    /// One keyvalue request; its answer, or the error.
    #[cfg(any(feature = "keyvalue", feature = "watch"))]
    fn kv(request: p::KvRequest) -> Result<p::KvAnswer, p::KvError> {
        Ctx::current().kv(request)
    }

    /// `wasi:keyvalue`, over the protocol. A bucket is its name: opening
    /// one asks the node nothing, so an unlisted bucket's error comes with
    /// its first operation.
    #[cfg(feature = "keyvalue")]
    pub(crate) mod keyvalue {
        pub(crate) mod store {
            pub(crate) use crate::bindings::wasi::keyvalue::store::{Error, KeyResponse};

            use bulkhead_native::app::Ctx;
            use bulkhead_native::hot::KvOp;

            use super::super::{kv, p};

            pub(crate) fn error(e: p::KvError) -> Error {
                match e {
                    p::KvError::AccessDenied => Error::AccessDenied,
                    p::KvError::NoSuchBucket => Error::NoSuchStore,
                    other => Error::Other(format!("{other:?}")),
                }
            }

            pub(crate) fn unexpected(answer: &p::KvAnswer) -> Error {
                Error::Other(format!("unexpected keyvalue answer {answer:?}"))
            }

            /// A bucket: its name.
            #[derive(Debug)]
            pub(crate) struct Bucket {
                pub(crate) name: String,
            }

            pub(crate) fn open(name: &str) -> Result<Bucket, Error> {
                Ok(Bucket {
                    name: name.to_string(),
                })
            }

            impl Bucket {
                pub(crate) fn request(&self, r: p::KvRequest) -> Result<p::KvAnswer, Error> {
                    kv(r).map_err(error)
                }

                /// A hot frame's operation (ROADMAP T7): the bucket and key
                /// borrowed, nothing built for the request.
                fn op(&self, op: KvOp, key: &str, value: &[u8]) -> Result<p::KvAnswer, Error> {
                    Ctx::current()
                        .kv_op(&self.name, op, key, value)
                        .map_err(error)
                }

                pub(crate) fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Error> {
                    match self.op(KvOp::Get, key, &[])? {
                        p::KvAnswer::Value(v) => Ok(v.map(|b| b.0)),
                        other => Err(unexpected(&other)),
                    }
                }

                pub(crate) fn set(&self, key: &str, value: &[u8]) -> Result<(), Error> {
                    self.op(KvOp::Set, key, value).map(|_| ())
                }

                pub(crate) fn delete(&self, key: &str) -> Result<(), Error> {
                    self.op(KvOp::Delete, key, &[]).map(|_| ())
                }

                pub(crate) fn exists(&self, key: &str) -> Result<bool, Error> {
                    match self.op(KvOp::Exists, key, &[])? {
                        p::KvAnswer::Exists(e) => Ok(e),
                        other => Err(unexpected(&other)),
                    }
                }

                pub(crate) fn list_keys(&self, cursor: Option<u64>) -> Result<KeyResponse, Error> {
                    let bucket = self.name.clone();
                    match self.request(p::KvRequest::Keys { bucket, cursor })? {
                        p::KvAnswer::Keys(keys, cursor) => Ok(KeyResponse { keys, cursor }),
                        other => Err(unexpected(&other)),
                    }
                }
            }
        }

        pub(crate) mod atomics {
            use super::store::{Bucket, Error, unexpected};
            use crate::host::native::p;

            pub(crate) fn increment(bucket: &Bucket, key: &str, delta: u64) -> Result<u64, Error> {
                let (name, key) = (bucket.name.clone(), key.to_string());
                match bucket.request(p::KvRequest::Increment {
                    bucket: name,
                    key,
                    delta,
                })? {
                    p::KvAnswer::Count(n) => Ok(n),
                    other => Err(unexpected(&other)),
                }
            }
        }

        pub(crate) mod batch {
            use super::store::{Bucket, Error, unexpected};
            use crate::host::native::p;

            /// A key `get_many` found, with its value.
            type Found = Option<(String, Vec<u8>)>;

            pub(crate) fn get_many(bucket: &Bucket, keys: &[String]) -> Result<Vec<Found>, Error> {
                let (name, asked) = (bucket.name.clone(), keys.to_vec());
                match bucket.request(p::KvRequest::GetMany {
                    bucket: name,
                    keys: asked,
                })? {
                    p::KvAnswer::Values(found) => Ok(keys
                        .iter()
                        .zip(found)
                        .map(|(k, v)| v.map(|b| (k.clone(), b.0)))
                        .collect()),
                    other => Err(unexpected(&other)),
                }
            }

            pub(crate) fn set_many(
                bucket: &Bucket,
                entries: &[(String, Vec<u8>)],
            ) -> Result<(), Error> {
                let entries = entries
                    .iter()
                    .map(|(k, v)| (k.clone(), p::Bytes(v.clone())))
                    .collect();
                bucket
                    .request(p::KvRequest::SetMany {
                        bucket: bucket.name.clone(),
                        entries,
                    })
                    .map(|_| ())
            }

            pub(crate) fn delete_many(bucket: &Bucket, keys: &[String]) -> Result<(), Error> {
                bucket
                    .request(p::KvRequest::DeleteMany {
                        bucket: bucket.name.clone(),
                        keys: keys.to_vec(),
                    })
                    .map(|_| ())
            }
        }
    }

    /// `bulkhead:watch`, over the protocol.
    #[cfg(feature = "watch")]
    pub(crate) mod watch {
        pub(crate) use crate::bindings::bulkhead::watch::watch::*;

        use super::{kv, p};

        fn error(e: p::KvError) -> Error {
            match e {
                p::KvError::AccessDenied => Error::AccessDenied,
                p::KvError::NoSuchBucket => Error::NoSuchBucket,
                p::KvError::LimitReached => Error::LimitReached,
                p::KvError::TooOld(now) => Error::TooOld(now),
                p::KvError::Conflict(found) => Error::Conflict(found),
                p::KvError::Unavailable => Error::Unavailable,
                p::KvError::Other(why) => Error::Other(why),
            }
        }

        fn unexpected(answer: &p::KvAnswer) -> Error {
            Error::Other(format!("unexpected keyvalue answer {answer:?}"))
        }

        fn ask(r: p::KvRequest) -> Result<p::KvAnswer, Error> {
            kv(r).map_err(error)
        }

        pub(crate) fn watch(
            bucket: &str,
            key: &str,
            prefix: bool,
            after: Option<u64>,
        ) -> Result<Started, Error> {
            match ask(p::KvRequest::Watch {
                bucket: bucket.to_string(),
                key: key.to_string(),
                prefix,
                after,
            })? {
                p::KvAnswer::Started { id, revision } => Ok(Started { id, revision }),
                other => Err(unexpected(&other)),
            }
        }

        pub(crate) fn unwatch(id: u64) -> Result<(), Error> {
            ask(p::KvRequest::Unwatch(id)).map(|_| ())
        }

        fn entry(bucket: &str, key: &str, linearizable: bool) -> Result<Option<Entry>, Error> {
            match ask(p::KvRequest::Entry {
                bucket: bucket.to_string(),
                key: key.to_string(),
                linearizable,
            })? {
                p::KvAnswer::Entry(e) => Ok(e.map(|(value, revision)| Entry {
                    value: value.0,
                    revision,
                })),
                other => Err(unexpected(&other)),
            }
        }

        pub(crate) fn get(bucket: &str, key: &str) -> Result<Option<Entry>, Error> {
            entry(bucket, key, false)
        }

        pub(crate) fn get_linearizable(bucket: &str, key: &str) -> Result<Option<Entry>, Error> {
            entry(bucket, key, true)
        }

        pub(crate) fn revision(bucket: &str) -> Result<u64, Error> {
            match ask(p::KvRequest::Revision {
                bucket: bucket.to_string(),
            })? {
                p::KvAnswer::Revision(r) => Ok(r),
                other => Err(unexpected(&other)),
            }
        }

        pub(crate) fn scan(bucket: &str, query: &ScanQuery) -> Result<ScanPage, Error> {
            let query = p::ScanQuery {
                prefix: query.prefix.clone(),
                start: query.start.clone(),
                end: query.end.clone(),
                reverse: query.reverse,
                limit: query.limit,
                cursor: query.cursor.clone(),
            };
            match ask(p::KvRequest::Scan {
                bucket: bucket.to_string(),
                query,
            })? {
                p::KvAnswer::Page(page) => Ok(ScanPage {
                    entries: page
                        .entries
                        .into_iter()
                        .map(|(key, value, revision)| KeyedEntry {
                            key,
                            value: value.0,
                            revision,
                        })
                        .collect(),
                    next: page.next,
                    revision: page.revision,
                }),
                other => Err(unexpected(&other)),
            }
        }

        pub(crate) fn compare_and_swap(
            bucket: &str,
            key: &str,
            expected: Option<u64>,
            value: &[u8],
        ) -> Result<u64, Error> {
            match ask(p::KvRequest::CompareAndSwap {
                bucket: bucket.to_string(),
                key: key.to_string(),
                expected,
                value: p::Bytes::from(value),
            })? {
                p::KvAnswer::Revision(r) => Ok(r),
                other => Err(unexpected(&other)),
            }
        }

        pub(crate) fn compare_and_delete(
            bucket: &str,
            key: &str,
            expected: u64,
        ) -> Result<u64, Error> {
            match ask(p::KvRequest::CompareAndDelete {
                bucket: bucket.to_string(),
                key: key.to_string(),
                expected,
            })? {
                p::KvAnswer::Revision(r) => Ok(r),
                other => Err(unexpected(&other)),
            }
        }
    }

    /// `bulkhead:groups`, over the protocol.
    #[cfg(feature = "groups")]
    pub(crate) mod groups {
        pub(crate) use crate::bindings::bulkhead::groups::groups::*;

        use super::{Ctx, envelope, p, wit, wit_pid};

        fn error(e: p::GroupsError) -> Error {
            match e {
                p::GroupsError::AccessDenied => Error::AccessDenied,
                p::GroupsError::LimitReached => Error::LimitReached,
            }
        }

        pub(crate) fn join(group: &str) -> Result<(), Error> {
            Ctx::current().join_group(group).map_err(error)
        }

        pub(crate) fn leave(group: &str) -> Result<(), Error> {
            Ctx::current().leave_group(group).map_err(error)
        }

        pub(crate) fn members(group: &str) -> Result<Vec<wit::Pid>, Error> {
            Ctx::current()
                .group_members(group)
                .map(|pids| pids.into_iter().map(wit_pid).collect())
                .map_err(error)
        }

        pub(crate) fn publish(group: &str, msg: &wit::Envelope) -> Result<u32, Error> {
            Ctx::current().publish(group, envelope(msg)).map_err(error)
        }
    }

    /// `wasi:config`, over the protocol.
    #[cfg(feature = "config")]
    pub(crate) mod config_store {
        pub(crate) use crate::bindings::wasi::config::store::*;

        use super::{Ctx, p};

        /// As the component host words a refusal.
        fn error(e: p::ConfigError) -> Error {
            match e {
                p::ConfigError::AccessDenied => Error::Io("access-denied".into()),
                p::ConfigError::Other(why) => Error::Io(why),
            }
        }

        pub(crate) fn get(key: &str) -> Result<Option<String>, Error> {
            Ctx::current().config(key).map_err(error)
        }

        pub(crate) fn get_all() -> Result<Vec<(String, String)>, Error> {
            Ctx::current().config_all().map_err(error)
        }
    }

    /// `bulkhead:secrets`, over the protocol.
    #[cfg(feature = "secrets")]
    pub(crate) mod secrets_store {
        pub(crate) use crate::bindings::bulkhead::secrets::store::*;

        use super::{Ctx, p};

        pub(crate) fn get(name: &str) -> Result<Vec<u8>, Error> {
            Ctx::current().secret(name).map_err(|e| match e {
                p::SecretError::AccessDenied => Error::AccessDenied,
                p::SecretError::NotFound => Error::NotFound,
                p::SecretError::Unavailable(why) => Error::Unavailable(why),
            })
        }
    }

    /// `bulkhead:metrics`, over the protocol: updates go with the
    /// callback's result, so they report no error here.
    #[cfg(feature = "metrics")]
    pub(crate) mod metrics {
        pub(crate) use crate::bindings::bulkhead::metrics::metrics::*;

        use super::{Ctx, p};

        pub(crate) fn register(
            kind: Kind,
            name: &str,
            help: &str,
            labels: &[(String, String)],
            buckets: &[f64],
        ) -> Result<u32, Error> {
            let kind = match kind {
                Kind::Counter => p::MetricKind::Counter,
                Kind::Gauge => p::MetricKind::Gauge,
                Kind::Histogram => p::MetricKind::Histogram,
            };
            let labels: Vec<(&str, &str)> = labels
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            Ctx::current()
                .register_metric(kind, name, help, &labels, buckets)
                .map_err(|e| match e {
                    p::MetricError::AccessDenied => Error::AccessDenied,
                    p::MetricError::Invalid(why) => Error::Invalid(why),
                    p::MetricError::LimitReached => Error::LimitReached,
                    p::MetricError::UnknownMetric => Error::UnknownMetric,
                })
        }

        pub(crate) fn add(metric: u32, delta: f64) -> Result<(), Error> {
            Ctx::current().metric_add(metric, delta);
            Ok(())
        }

        pub(crate) fn set(metric: u32, value: f64) -> Result<(), Error> {
            Ctx::current().metric_set(metric, value);
            Ok(())
        }

        pub(crate) fn observe(metric: u32, value: f64) -> Result<(), Error> {
            Ctx::current().metric_observe(metric, value);
            Ok(())
        }
    }
}
