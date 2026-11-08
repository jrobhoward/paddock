//! Keyvalue buckets, through `wasi:keyvalue` (cargo feature `keyvalue`).
//!
//! Declare the `wasi:keyvalue` capability and each bucket in the manifest;
//! an instance can open only the buckets listed there. Buckets are
//! host-owned: `memory` buckets outlive instance crashes, `disk` buckets also
//! survive node restarts. Each operation is one transaction.
//!
//! ```ignore
//! use bulkhead_sdk::kv::Bucket;
//!
//! let orders = Bucket::open("orders")?;
//! orders.set_value("order:42", &order)?;          // CBOR
//! let seen = orders.increment("orders-seen", 1)?;
//! ```

use std::fmt;

use bulkhead_native::app::Ctx;
use bulkhead_native::hot::KvOp;
use bulkhead_native::protocol as p;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Why a keyvalue operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KvError {
    /// The operator did not grant `wasi:keyvalue`, or the manifest does not
    /// list the bucket.
    AccessDenied,
    /// The host does not know the bucket.
    NoSuchStore,
    /// Storage failed, or a quota was exceeded.
    Other(String),
    /// A stored value is not the CBOR of the type asked for, or a value
    /// would not encode.
    Codec(String),
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("access denied"),
            Self::NoSuchStore => f.write_str("no such bucket"),
            Self::Other(e) => write!(f, "storage: {e}"),
            Self::Codec(e) => write!(f, "value encoding: {e}"),
        }
    }
}

impl std::error::Error for KvError {}

impl From<p::KvError> for KvError {
    fn from(e: p::KvError) -> Self {
        match e {
            p::KvError::AccessDenied => Self::AccessDenied,
            p::KvError::NoSuchBucket => Self::NoSuchStore,
            other => Self::Other(format!("{other:?}")),
        }
    }
}

fn unexpected(answer: &p::KvAnswer) -> KvError {
    KvError::Other(format!("unexpected keyvalue answer {answer:?}"))
}

/// A bucket: its name. Opening one asks the node nothing, so an unlisted
/// bucket's error comes with its first operation.
#[derive(Debug)]
pub struct Bucket {
    name: String,
}

impl Bucket {
    /// The bucket the manifest lists as `name`.
    ///
    /// # Errors
    ///
    /// None today; each operation reports [`KvError::AccessDenied`] for a
    /// bucket the manifest does not list or without the capability granted.
    pub fn open(name: &str) -> Result<Self, KvError> {
        Ok(Self {
            name: name.to_string(),
        })
    }

    fn request(&self, request: p::KvRequest) -> Result<p::KvAnswer, KvError> {
        Ok(Ctx::current().kv(request)?)
    }

    /// A hot frame's operation (ROADMAP T7): the bucket and key borrowed,
    /// nothing built for the request.
    fn op(&self, op: KvOp, key: &str, value: &[u8]) -> Result<p::KvAnswer, KvError> {
        Ok(Ctx::current().kv_op(&self.name, op, key, value)?)
    }

    /// The bytes stored under `key`.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        match self.op(KvOp::Get, key, &[])? {
            p::KvAnswer::Value(v) => Ok(v.map(|b| b.0)),
            other => Err(unexpected(&other)),
        }
    }

    /// Store `value` under `key`.
    ///
    /// # Errors
    ///
    /// The host's error, including [`KvError::Other`] past the bucket's
    /// quota.
    pub fn set(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        self.op(KvOp::Set, key, value).map(|_| ())
    }

    /// Remove `key`; removing a missing key is not an error.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn delete(&self, key: &str) -> Result<(), KvError> {
        self.op(KvOp::Delete, key, &[]).map(|_| ())
    }

    /// Whether `key` is set.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn exists(&self, key: &str) -> Result<bool, KvError> {
        match self.op(KvOp::Exists, key, &[])? {
            p::KvAnswer::Exists(e) => Ok(e),
            other => Err(unexpected(&other)),
        }
    }

    /// Every key, in order, fetching page after page.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn keys(&self) -> Result<Vec<String>, KvError> {
        let mut keys = Vec::new();
        let mut cursor = None;
        loop {
            let bucket = self.name.clone();
            match self.request(p::KvRequest::Keys { bucket, cursor })? {
                p::KvAnswer::Keys(page, next) => {
                    keys.extend(page);
                    match next {
                        Some(next) => cursor = Some(next),
                        None => return Ok(keys),
                    }
                }
                other => return Err(unexpected(&other)),
            }
        }
    }

    /// Add `delta` to the counter at `key` (zero if unset) and return the
    /// new value. Counters are stored as decimal text.
    ///
    /// # Errors
    ///
    /// The host's error, including [`KvError::Other`] for a value that is
    /// not a counter.
    pub fn increment(&self, key: &str, delta: u64) -> Result<u64, KvError> {
        match self.request(p::KvRequest::Increment {
            bucket: self.name.clone(),
            key: key.to_string(),
            delta,
        })? {
            p::KvAnswer::Count(n) => Ok(n),
            other => Err(unexpected(&other)),
        }
    }

    /// The values of several keys, in order; `None` for keys not set.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn get_many(&self, keys: &[String]) -> Result<Vec<Option<Vec<u8>>>, KvError> {
        match self.request(p::KvRequest::GetMany {
            bucket: self.name.clone(),
            keys: keys.to_vec(),
        })? {
            p::KvAnswer::Values(found) => Ok(found.into_iter().map(|v| v.map(|b| b.0)).collect()),
            other => Err(unexpected(&other)),
        }
    }

    /// Store several values in one transaction.
    ///
    /// # Errors
    ///
    /// The host's error; on error nothing is stored.
    pub fn set_many(&self, entries: &[(String, Vec<u8>)]) -> Result<(), KvError> {
        let entries = entries
            .iter()
            .map(|(k, v)| (k.clone(), p::Bytes(v.clone())))
            .collect();
        self.request(p::KvRequest::SetMany {
            bucket: self.name.clone(),
            entries,
        })
        .map(|_| ())
    }

    /// Remove several keys in one transaction.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn delete_many(&self, keys: &[String]) -> Result<(), KvError> {
        self.request(p::KvRequest::DeleteMany {
            bucket: self.name.clone(),
            keys: keys.to_vec(),
        })
        .map(|_| ())
    }

    /// The value under `key`, decoded from CBOR.
    ///
    /// # Errors
    ///
    /// [`KvError::Codec`] if the bytes are not `T`, and the host's error.
    pub fn get_value<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, KvError> {
        self.get(key)?
            .map(|bytes| {
                ciborium::from_reader(bytes.as_slice()).map_err(|e| KvError::Codec(e.to_string()))
            })
            .transpose()
    }

    /// Store `value` under `key` as CBOR.
    ///
    /// # Errors
    ///
    /// [`KvError::Codec`] if `value` does not encode, and the host's error.
    pub fn set_value<T: Serialize + ?Sized>(&self, key: &str, value: &T) -> Result<(), KvError> {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).map_err(|e| KvError::Codec(e.to_string()))?;
        self.set(key, &bytes)
    }
}
