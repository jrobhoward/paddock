//! Published buckets (ROADMAP T4, spike S31): a shared bucket with
//! `publish` set is checked for changes that often, and each change is
//! read into a snapshot (`bulkhead_native::snapshot`) in a sealed memfd.
//! Native instances that read the bucket get each one through their
//! descriptor socket (`instance::native`) and read the bucket there
//! without asking the node; components keep their host calls.
//!
//! One thread per store does it, started with the first published bucket
//! an instance reads, and ending with the store: a thread, not a task, so
//! it does not end with whichever tokio runtime happened to start it. It
//! reads through [`KvStore::read`], as host functions bound synchronously
//! do.
//!
//! A snapshot is read a page at a time, not under one lock, and is tagged
//! with the bucket's revision when its first page was read: every write
//! at or below that revision is in it, and some later ones may be too. Its
//! memory is the node's (the node writes it, so the node's cgroup is
//! charged), held while the latest version, or an app's mapping of it,
//! lives. Not on systems without native apps.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::storage::{BucketHandle, KvStore, Op, Outcome, SHARED, ScanQuery};
use bulkhead_bundle::manifest::Durability;

/// One snapshot of a published bucket.
#[derive(Debug)]
pub(crate) struct Version {
    /// Every write at or below it is in the snapshot.
    pub(crate) revision: u64,
    /// The sealed memfd holding it.
    #[cfg(target_os = "linux")]
    pub(crate) memfd: std::os::fd::OwnedFd,
    /// Its size.
    pub(crate) bytes: usize,
}

/// A published bucket's latest snapshot, `None` until the first is made.
pub(crate) type Versions = watch::Receiver<Option<Arc<Version>>>;

/// The published buckets an instance reads: each one's snapshots and the
/// revision last passed to its running process, and a receiver that
/// changes with every new snapshot of any bucket.
#[derive(Debug)]
pub(crate) struct Publications {
    pub(crate) buckets: Vec<(String, Versions, Option<u64>)>,
    pub(crate) changed: watch::Receiver<u64>,
}

/// One published bucket, as the thread keeps it.
#[derive(Debug)]
struct Publisher {
    tx: watch::Sender<Option<Arc<Version>>>,
    every: Duration,
    /// When to look for changes next.
    due: Instant,
    /// The bucket revision the latest snapshot was made at.
    published: Option<u64>,
}

/// The node's published buckets, each started on first use, and the
/// thread that publishes them.
#[derive(Debug)]
pub(crate) struct Publishers {
    buckets: Mutex<HashMap<String, Publisher>>,
    thread: std::sync::OnceLock<()>,
    /// Moves on with every new snapshot of any bucket: an instance
    /// waiting on it looks at its own buckets' versions then.
    generation: watch::Sender<u64>,
}

impl Default for Publishers {
    fn default() -> Self {
        Self {
            buckets: Mutex::default(),
            thread: std::sync::OnceLock::new(),
            generation: watch::channel(0).0,
        }
    }
}

impl Publishers {
    /// A receiver that changes with every new snapshot of any bucket.
    pub(crate) fn generation(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }
}

/// A shared bucket's handle for reading it whole.
fn shared_handle(store: &KvStore, name: &str) -> Option<BucketHandle> {
    let bucket = store.shared(name)?;
    let space = if bucket.durability == Durability::Replicated {
        crate::replication::SPACE
    } else {
        SHARED
    };
    Some(BucketHandle {
        space: space.to_string(),
        name: name.to_string(),
        durability: bucket.durability,
        quota: bucket.quota,
        writable: false,
        read_concurrency: bucket.read_concurrency,
    })
}

/// The longest the thread sleeps between looks, so a newly published
/// bucket with a short interval is not kept waiting by a long one.
const LONGEST_SLEEP: Duration = Duration::from_millis(100);

impl KvStore {
    /// The snapshots of shared bucket `name`, if it is published; its
    /// publishing starts with the first call.
    pub(crate) fn published(self: &Arc<Self>, name: &str) -> Option<Versions> {
        let every = self.shared(name)?.publish?;
        if cfg!(not(target_os = "linux")) {
            return None;
        }
        let rx = {
            let mut buckets = self
                .publishers
                .buckets
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            buckets
                .entry(name.to_string())
                .or_insert_with(|| Publisher {
                    tx: watch::channel(None).0,
                    every,
                    due: Instant::now(),
                    published: None,
                })
                .tx
                .subscribe()
        };
        self.publishers.thread.get_or_init(|| {
            let store = Arc::downgrade(self);
            if let Err(e) = std::thread::Builder::new()
                .name("bulkhead-publish".into())
                .spawn(move || publish(&store))
            {
                tracing::warn!(error = %e, "no publishing thread; published buckets are read from the node");
            }
        });
        Some(rx)
    }
}

/// Publish every published bucket that changed, each at its interval, for
/// as long as the store lives.
fn publish(store: &Weak<KvStore>) {
    loop {
        let Some(store) = store.upgrade() else {
            return;
        };
        let now = Instant::now();
        let due: Vec<(String, Option<u64>)> = {
            let mut buckets = store
                .publishers
                .buckets
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            buckets
                .iter_mut()
                .filter(|(_, p)| p.due <= now)
                .map(|(name, p)| {
                    p.due = now + p.every;
                    (name.clone(), p.published)
                })
                .collect()
        };
        for (name, published) in due {
            publish_one(&store, &name, published);
        }
        let next = {
            let buckets = store
                .publishers
                .buckets
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            buckets.values().map(|p| p.due).min()
        };
        drop(store);
        let sleep = next.map_or(LONGEST_SLEEP, |due| {
            due.saturating_duration_since(Instant::now())
        });
        std::thread::sleep(sleep.min(LONGEST_SLEEP));
    }
}

/// Publish `name` if it changed since the snapshot made at `published`.
fn publish_one(store: &KvStore, name: &str, published: Option<u64>) {
    let Some(handle) = shared_handle(store, name) else {
        return;
    };
    let now = match store.read(&handle, Op::Revision(String::new())) {
        Ok(Outcome::Revision { bucket, .. }) => bucket,
        _ => return,
    };
    if published == Some(now) {
        return;
    }
    let version = match snapshot(store, &handle) {
        Ok(version) => version,
        Err(e) => {
            tracing::warn!(bucket = %name, error = %e, "bucket not published");
            return;
        }
    };
    tracing::debug!(
        bucket = %name,
        revision = version.revision,
        bytes = version.bytes,
        "bucket published"
    );
    {
        let mut buckets = store
            .publishers
            .buckets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(p) = buckets.get_mut(name) {
            p.published = Some(now);
            p.tx.send_replace(Some(Arc::new(version)));
        }
    }
    store.publishers.generation.send_modify(|g| *g += 1);
}

/// Read `handle` whole into a sealed snapshot.
fn snapshot(store: &KvStore, handle: &BucketHandle) -> Result<Version, String> {
    let mut entries = Vec::new();
    let mut revision = None;
    let mut cursor = None;
    loop {
        let query = ScanQuery {
            cursor: cursor.take(),
            ..ScanQuery::default()
        };
        let Outcome::Page {
            entries: page,
            next,
            revision: at,
        } = store
            .read(handle, Op::Scan(query))
            .map_err(|e| format!("{e:?}"))?
        else {
            return Err("the scan answered something else".into());
        };
        revision.get_or_insert(at);
        entries.extend(page.into_iter().map(|(k, v, _)| (k, v)));
        match next {
            Some(key) => cursor = Some(key),
            None => break,
        }
    }
    seal(&handle.name, revision.unwrap_or(0), &entries)
}

#[cfg(target_os = "linux")]
fn seal(name: &str, revision: u64, entries: &[(String, Vec<u8>)]) -> Result<Version, String> {
    let bytes = bulkhead_native::snapshot::build(revision, entries);
    let memfd = bulkhead_native::channel::sealed(&format!("bulkhead-published-{name}"), &bytes)
        .map_err(|e| e.to_string())?;
    Ok(Version {
        revision,
        memfd,
        bytes: bytes.len(),
    })
}

#[cfg(not(target_os = "linux"))]
fn seal(_name: &str, _revision: u64, _entries: &[(String, Vec<u8>)]) -> Result<Version, String> {
    Err("published buckets need Linux".into())
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod publish_tests;
