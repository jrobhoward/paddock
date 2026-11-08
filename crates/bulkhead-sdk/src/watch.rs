//! Revisioned reads, compare-and-swap and change watches on keyvalue
//! buckets, through `bulkhead:watch` (cargo feature `watch`).
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
//! use bulkhead_sdk::watch::{self, Change};
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

use bulkhead_native::app::Ctx;
use bulkhead_native::protocol as p;
use serde::Deserialize;

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

impl From<p::KvError> for WatchError {
    fn from(e: p::KvError) -> Self {
        match e {
            p::KvError::AccessDenied => Self::AccessDenied,
            p::KvError::NoSuchBucket => Self::NoSuchBucket,
            p::KvError::LimitReached => Self::LimitReached,
            p::KvError::TooOld(now) => Self::TooOld(now),
            p::KvError::Conflict(found) => Self::Conflict(found),
            p::KvError::Unavailable => Self::Unavailable,
            p::KvError::Other(why) => Self::Other(why),
        }
    }
}

fn unexpected(answer: &p::KvAnswer) -> WatchError {
    WatchError::Other(format!("unexpected keyvalue answer {answer:?}"))
}

fn ask(request: p::KvRequest) -> Result<p::KvAnswer, WatchError> {
    Ok(Ctx::current().kv(request)?)
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
    /// Whether the notice carries the key's value in [`Change::value`] (read
    /// by the node when it delivered the notice, with `revision` the
    /// value's own). [`Change::current`] reads it either way.
    #[serde(default)]
    pub carried: bool,
    /// The key's value when `carried`; `None` if it was deleted.
    #[serde(default)]
    pub value: Option<Vec<u8>>,
}

impl Change {
    /// The key's value now: the one the notice carries, or read with
    /// [`get`] when it carries none. `None` if the key was deleted.
    ///
    /// # Errors
    ///
    /// As for [`get`], when it has to read.
    pub fn current(&self) -> Result<Option<Entry>, WatchError> {
        if self.carried {
            return Ok(self.value.clone().map(|value| Entry {
                value,
                revision: self.revision,
            }));
        }
        get(&self.bucket, &self.key)
    }

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
    ask(p::KvRequest::Unwatch(id)).map(|_| ())
}

fn entry(bucket: &str, key: &str, linearizable: bool) -> Result<Option<Entry>, WatchError> {
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

/// The value of `key` and its revision.
///
/// # Errors
///
/// As for [`WatchError`], other than `TooOld` and `Conflict`.
pub fn get(bucket: &str, key: &str) -> Result<Option<Entry>, WatchError> {
    entry(bucket, key, false)
}

/// As [`get`], but linearizable: on a replicated bucket, served by the HA
/// pair's primary, so it reflects every write committed before the call.
/// [`get`] reads this node's copy, which may trail the primary slightly.
///
/// # Errors
///
/// As for [`get`]; `Unavailable` when no primary can serve it.
pub fn get_linearizable(bucket: &str, key: &str) -> Result<Option<Entry>, WatchError> {
    entry(bucket, key, true)
}

/// The bucket's revision.
///
/// # Errors
///
/// As for [`get`].
pub fn revision(bucket: &str) -> Result<u64, WatchError> {
    match ask(p::KvRequest::Revision {
        bucket: bucket.to_string(),
    })? {
        p::KvAnswer::Revision(r) => Ok(r),
        other => Err(unexpected(&other)),
    }
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

/// Delete `key` if its revision is `expected`. Returns the bucket's new
/// revision.
///
/// # Errors
///
/// [`WatchError::Conflict`] with the key's revision if it differs.
pub fn compare_and_delete(bucket: &str, key: &str, expected: u64) -> Result<u64, WatchError> {
    match ask(p::KvRequest::CompareAndDelete {
        bucket: bucket.to_string(),
        key: key.to_string(),
        expected,
    })? {
        p::KvAnswer::Revision(r) => Ok(r),
        other => Err(unexpected(&other)),
    }
}

/// One entry a scan found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedEntry {
    /// The key.
    pub key: String,
    /// Its value.
    pub value: Vec<u8>,
    /// The revision it was written at.
    pub revision: u64,
}

/// One page of a [`Scan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// In key order, or downward with [`Scan::reverse`].
    pub entries: Vec<KeyedEntry>,
    /// Where the next page starts (for [`Scan::page`]); `None` when the
    /// scan is complete.
    pub next: Option<String>,
    /// The bucket's revision when the page was read: [`resume`] from it to
    /// see every change after.
    pub revision: u64,
}

/// A bucket's entries in key order (the bytes of their UTF-8), like ETS's
/// `ordered_set`: every key, or those with a prefix or between two keys,
/// upward or downward. Read a page at a time with [`Scan::page`], or
/// iterate over them all with [`Scan::entries`]. Each page is read at one
/// revision; a scan of many pages is not one snapshot.
///
/// ```ignore
/// // Every order of customer 42, newest key first.
/// for entry in watch::scan("orders").prefix("c42/").reverse().entries() {
///     let entry = entry?;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Scan {
    bucket: String,
    query: p::ScanQuery,
}

/// A scan of `bucket`; every key, upward, until narrowed.
#[must_use]
pub fn scan(bucket: &str) -> Scan {
    Scan {
        bucket: bucket.to_string(),
        query: p::ScanQuery {
            prefix: None,
            start: None,
            end: None,
            reverse: false,
            limit: 0,
            cursor: None,
        },
    }
}

impl Scan {
    /// Only keys starting with `prefix`.
    #[must_use]
    pub fn prefix(mut self, prefix: &str) -> Self {
        self.query.prefix = Some(prefix.to_string());
        self
    }

    /// Only keys from `key` on (inclusive).
    #[must_use]
    pub fn start(mut self, key: &str) -> Self {
        self.query.start = Some(key.to_string());
        self
    }

    /// Only keys before `key` (exclusive).
    #[must_use]
    pub fn end(mut self, key: &str) -> Self {
        self.query.end = Some(key.to_string());
        self
    }

    /// Downward, from the last key in range.
    #[must_use]
    pub fn reverse(mut self) -> Self {
        self.query.reverse = true;
        self
    }

    /// At most `n` entries per page (the host caps it at 1,000).
    #[must_use]
    pub fn limit(mut self, n: u32) -> Self {
        self.query.limit = n;
        self
    }

    /// One page, from the start or from a previous page's
    /// [`next`](Page::next).
    ///
    /// # Errors
    ///
    /// As for [`get`].
    pub fn page(&self, cursor: Option<&str>) -> Result<Page, WatchError> {
        let mut query = self.query.clone();
        query.cursor = cursor.map(str::to_string);
        let answer = ask(p::KvRequest::Scan {
            bucket: self.bucket.clone(),
            query,
        })?;
        let p::KvAnswer::Page(page) = answer else {
            return Err(unexpected(&answer));
        };
        Ok(Page {
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
        })
    }

    /// Every entry, reading a page at a time as the iterator needs it. An
    /// error ends the iteration after it.
    #[must_use]
    pub fn entries(self) -> Entries {
        Entries {
            scan: self,
            buffered: std::collections::VecDeque::new(),
            cursor: None,
            done: false,
        }
    }
}

/// The entries of a [`Scan`], from [`Scan::entries`].
#[derive(Debug)]
pub struct Entries {
    scan: Scan,
    buffered: std::collections::VecDeque<KeyedEntry>,
    cursor: Option<String>,
    done: bool,
}

impl Iterator for Entries {
    type Item = Result<KeyedEntry, WatchError>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.buffered.is_empty() {
            if self.done {
                return None;
            }
            match self.scan.page(self.cursor.as_deref()) {
                Ok(page) => {
                    self.buffered.extend(page.entries);
                    self.done = page.next.is_none();
                    self.cursor = page.next;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
        self.buffered.pop_front().map(Ok)
    }
}

fn one(scan: &Scan) -> Result<Option<KeyedEntry>, WatchError> {
    Ok(scan.page(None)?.entries.into_iter().next())
}

/// The entry with the smallest key (ETS `first`).
///
/// # Errors
///
/// As for [`get`].
pub fn first(bucket: &str) -> Result<Option<KeyedEntry>, WatchError> {
    one(&scan(bucket).limit(1))
}

/// The entry with the largest key (ETS `last`).
///
/// # Errors
///
/// As for [`get`].
pub fn last(bucket: &str) -> Result<Option<KeyedEntry>, WatchError> {
    one(&scan(bucket).reverse().limit(1))
}

/// The entry with the smallest key after `key`, which need not exist (ETS
/// `next`).
///
/// # Errors
///
/// As for [`get`].
pub fn next(bucket: &str, key: &str) -> Result<Option<KeyedEntry>, WatchError> {
    let page = scan(bucket).start(key).limit(2).page(None)?;
    Ok(page.entries.into_iter().find(|e| e.key.as_str() > key))
}

/// The entry with the largest key before `key`, which need not exist (ETS
/// `prev`).
///
/// # Errors
///
/// As for [`get`].
pub fn prev(bucket: &str, key: &str) -> Result<Option<KeyedEntry>, WatchError> {
    one(&scan(bucket).end(key).reverse().limit(1))
}
