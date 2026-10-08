//! Keyvalue buckets, through `wasi:keyvalue` (cargo feature `keyvalue`).
//!
//! Declare the `wasi:keyvalue` capability and each bucket in the manifest;
//! an instance can open only the buckets listed there. Buckets are
//! host-owned: `memory` buckets outlive instance crashes, `disk` buckets also
//! survive node restarts. Each operation is one transaction.
//!
//! ```ignore
//! use paddock_sdk::kv::Bucket;
//!
//! let orders = Bucket::open("orders")?;
//! orders.set_value("order:42", &order)?;          // CBOR
//! let seen = orders.increment("orders-seen", 1)?;
//! ```

use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::bindings::wasi::keyvalue::{atomics, batch, store};

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

impl From<store::Error> for KvError {
    fn from(e: store::Error) -> Self {
        match e {
            store::Error::AccessDenied => Self::AccessDenied,
            store::Error::NoSuchStore => Self::NoSuchStore,
            store::Error::Other(msg) => Self::Other(msg),
        }
    }
}

/// An open bucket. Dropping it closes the handle; the data stays.
#[derive(Debug)]
pub struct Bucket {
    inner: store::Bucket,
}

impl Bucket {
    /// Open the bucket the manifest lists as `name`.
    ///
    /// # Errors
    ///
    /// [`KvError::AccessDenied`] for a bucket the manifest does not list or
    /// without the capability granted.
    pub fn open(name: &str) -> Result<Self, KvError> {
        Ok(Self {
            inner: store::open(name)?,
        })
    }

    /// The bytes stored under `key`.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        Ok(self.inner.get(key)?)
    }

    /// Store `value` under `key`.
    ///
    /// # Errors
    ///
    /// The host's error, including [`KvError::Other`] past the bucket's
    /// quota.
    pub fn set(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        Ok(self.inner.set(key, value)?)
    }

    /// Remove `key`; removing a missing key is not an error.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn delete(&self, key: &str) -> Result<(), KvError> {
        Ok(self.inner.delete(key)?)
    }

    /// Whether `key` is set.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn exists(&self, key: &str) -> Result<bool, KvError> {
        Ok(self.inner.exists(key)?)
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
            let page = self.inner.list_keys(cursor)?;
            keys.extend(page.keys);
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(keys),
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
        Ok(atomics::increment(&self.inner, key, delta)?)
    }

    /// The values of several keys, in order; `None` for keys not set.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn get_many(&self, keys: &[String]) -> Result<Vec<Option<Vec<u8>>>, KvError> {
        let found = batch::get_many(&self.inner, keys)?;
        Ok(found.into_iter().map(|kv| kv.map(|(_, v)| v)).collect())
    }

    /// Store several values in one transaction.
    ///
    /// # Errors
    ///
    /// The host's error; on error nothing is stored.
    pub fn set_many(&self, entries: &[(String, Vec<u8>)]) -> Result<(), KvError> {
        Ok(batch::set_many(&self.inner, entries)?)
    }

    /// Remove several keys in one transaction.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn delete_many(&self, keys: &[String]) -> Result<(), KvError> {
        Ok(batch::delete_many(&self.inner, keys)?)
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
