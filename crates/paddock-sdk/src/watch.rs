//! Revisioned reads, compare-and-swap and change watches on keyvalue
//! buckets, through `paddock:watch` (cargo feature `watch`).
//!
//! Covered by the `wasi:keyvalue` capability: a bucket works here exactly
//! when [`kv::Bucket::open`](crate::kv) would open it. Every change raises
//! the bucket's revision and stamps the keys it wrote. A watch delivers one
//! notice per changed key to `handle_info` as [`Info::Event`]; turn it into
//! a [`Change`] with [`Change::from_info`] and read the value with [`get`].
//! Notices are conflated per key, so a slow instance misses intermediate
//! values but always sees the latest.
//!
//! ```ignore
//! use paddock_sdk::watch::{self, Change};
//!
//! // In init: watch every point, from now.
//! let started = watch::watch_prefix("points", "pump-")?;
//!
//! // In handle_info:
//! if let Some(change) = Change::from_info(&info) {
//!     let now = watch::get(&change.bucket, &change.key)?;
//! }
//!
//! // Lock-free update: retry on conflict.
//! let entry = watch::get("points", "pump-1")?;
//! watch::compare_and_swap("points", "pump-1", entry.map(|e| e.revision), b"on")?;
//! ```

use std::fmt;

use serde::Deserialize;

use crate::bindings::paddock::watch::watch as wit;
use crate::types::Info;

/// The host event a change notice arrives as.
pub const EVENT: &str = "kv-changed";

/// Why an operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchError {
    /// The capability is not granted, the manifest does not list the
    /// bucket, or a write went to a read-only shared bucket.
    AccessDenied,
    /// The node does not configure this shared bucket.
    NoSuchBucket,
    /// Over `limits.watches_max`.
    LimitReached,
    /// The bucket cannot replay changes after that revision. Reread the
    /// watched keys, then watch from this revision (the current one).
    TooOld(u64),
    /// Compare-and-swap found the key at this revision (`None`: absent).
    Conflict(Option<u64>),
    /// A replicated bucket with no primary that can take it now, or a
    /// write whose outcome is unknown (its primary stepped down first).
    Unavailable,
    /// Storage failed, or a quota was exceeded.
    Other(String),
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("access denied"),
            Self::NoSuchBucket => f.write_str("no such bucket"),
            Self::LimitReached => f.write_str("watch limit reached"),
            Self::TooOld(now) => write!(f, "revision too old (bucket at {now})"),
            Self::Conflict(Some(r)) => write!(f, "conflict (key at {r})"),
            Self::Conflict(None) => f.write_str("conflict (key absent)"),
            Self::Unavailable => f.write_str("unavailable"),
            Self::Other(e) => write!(f, "storage: {e}"),
        }
    }
}

impl std::error::Error for WatchError {}

impl From<wit::Error> for WatchError {
    fn from(e: wit::Error) -> Self {
        match e {
            wit::Error::AccessDenied => Self::AccessDenied,
            wit::Error::NoSuchBucket => Self::NoSuchBucket,
            wit::Error::LimitReached => Self::LimitReached,
            wit::Error::TooOld(now) => Self::TooOld(now),
            wit::Error::Conflict(found) => Self::Conflict(found),
            wit::Error::Unavailable => Self::Unavailable,
            wit::Error::Other(e) => Self::Other(e),
        }
    }
}

/// A value and the revision it was written at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The value.
    pub value: Vec<u8>,
    /// Its revision (0 for data older than revisions).
    pub revision: u64,
}

/// A started watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Started {
    /// The watch's id, for [`unwatch`] and in its notices.
    pub id: u64,
    /// The bucket's revision when it started: read the watched keys, and
    /// resume from here after a restart.
    pub revision: u64,
}

/// A change notice.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Change {
    /// The watch that matched.
    pub watch: u64,
    /// The bucket.
    pub bucket: String,
    /// The key that changed (it may have been deleted).
    pub key: String,
    /// The key's latest revision when the notice was taken.
    pub revision: u64,
}

impl Change {
    /// The change an [`Info`] carries, if it is a change notice.
    #[must_use]
    pub fn from_info(info: &Info) -> Option<Self> {
        match info {
            Info::Event { name, msg } if name == EVENT => msg.decode().ok(),
            _ => None,
        }
    }
}

fn start(bucket: &str, key: &str, prefix: bool, after: Option<u64>) -> Result<Started, WatchError> {
    let s = wit::watch(bucket, key, prefix, after)?;
    Ok(Started {
        id: s.id,
        revision: s.revision,
    })
}

/// Watch one key, from now.
///
/// # Errors
///
/// As for [`WatchError`], other than `TooOld` and `Conflict`.
pub fn watch(bucket: &str, key: &str) -> Result<Started, WatchError> {
    start(bucket, key, false, None)
}

/// Watch every key starting with `prefix` (all keys for `""`), from now.
///
/// # Errors
///
/// As for [`watch`].
pub fn watch_prefix(bucket: &str, prefix: &str) -> Result<Started, WatchError> {
    start(bucket, prefix, true, None)
}

/// Watch a key (or prefix), first replaying every matching key changed
/// after revision `after`.
///
/// # Errors
///
/// [`WatchError::TooOld`] if the bucket cannot replay from `after`; as for
/// [`watch`] otherwise.
pub fn resume(bucket: &str, key: &str, prefix: bool, after: u64) -> Result<Started, WatchError> {
    start(bucket, key, prefix, Some(after))
}

/// End a watch.
///
/// # Errors
///
/// None today; reserved.
pub fn unwatch(id: u64) -> Result<(), WatchError> {
    Ok(wit::unwatch(id)?)
}

/// The value of `key` and its revision.
///
/// # Errors
///
/// As for [`WatchError`], other than `TooOld` and `Conflict`.
pub fn get(bucket: &str, key: &str) -> Result<Option<Entry>, WatchError> {
    Ok(wit::get(bucket, key)?.map(|e| Entry {
        value: e.value,
        revision: e.revision,
    }))
}

/// As [`get`], but linearizable: on a replicated bucket, served by the HA
/// pair's primary, so it reflects every write committed before the call.
/// [`get`] reads this node's copy, which may trail the primary slightly.
///
/// # Errors
///
/// As for [`get`]; `Unavailable` when no primary can serve it.
pub fn get_linearizable(bucket: &str, key: &str) -> Result<Option<Entry>, WatchError> {
    Ok(wit::get_linearizable(bucket, key)?.map(|e| Entry {
        value: e.value,
        revision: e.revision,
    }))
}

/// The bucket's revision.
///
/// # Errors
///
/// As for [`get`].
pub fn revision(bucket: &str) -> Result<u64, WatchError> {
    Ok(wit::revision(bucket)?)
}

/// Set `key` if its revision is `expected` (`None`: only if absent).
/// Returns the new revision.
///
/// # Errors
///
/// [`WatchError::Conflict`] with the key's revision if it differs.
pub fn compare_and_swap(
    bucket: &str,
    key: &str,
    expected: Option<u64>,
    value: &[u8],
) -> Result<u64, WatchError> {
    Ok(wit::compare_and_swap(bucket, key, expected, value)?)
}

/// Delete `key` if its revision is `expected`. Returns the bucket's new
/// revision.
///
/// # Errors
///
/// [`WatchError::Conflict`] with the key's revision if it differs.
pub fn compare_and_delete(bucket: &str, key: &str, expected: u64) -> Result<u64, WatchError> {
    Ok(wit::compare_and_delete(bucket, key, expected)?)
}
