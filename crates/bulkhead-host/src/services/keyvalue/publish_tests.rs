#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use crate::config::SharedBucket;
use std::collections::BTreeMap;
use std::time::Instant;

fn store(publish: Option<Duration>) -> Arc<KvStore> {
    let shared = BTreeMap::from([(
        "board".to_string(),
        SharedBucket {
            durability: Durability::Memory,
            quota: None,
            read_concurrency: false,
            publish,
        },
    )]);
    Arc::new(KvStore::open(None, shared).unwrap())
}

async fn set(store: &KvStore, key: &str, value: &str) {
    let handle = shared_handle(store, "board").unwrap();
    store
        .run(&handle, Op::Set(key.into(), value.as_bytes().to_vec()))
        .await
        .unwrap();
}

/// The first version at or past `revision`, within two seconds.
async fn version_at(versions: &mut Versions, revision: u64) -> Arc<Version> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(v) = versions.borrow_and_update().clone()
            && v.revision >= revision
        {
            return v;
        }
        assert!(Instant::now() < deadline, "no version at {revision}");
        let _ = tokio::time::timeout(Duration::from_millis(50), versions.changed()).await;
    }
}

#[cfg(target_os = "linux")]
fn read(version: &Version, key: &str) -> Option<String> {
    let mapped = bulkhead_native::channel::Mapped::open(&version.memfd).unwrap();
    let snap = bulkhead_native::snapshot::Snapshot::parse(mapped.bytes()).unwrap();
    snap.get(key.as_bytes())
        .map(|v| String::from_utf8(v.to_vec()).unwrap())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn published____writes____each_in_a_later_snapshot() {
    let store = store(Some(Duration::from_millis(10)));
    set(&store, "a", "one").await;
    let mut versions = store.published("board").unwrap();

    let first = version_at(&mut versions, 1).await;
    set(&store, "a", "two").await;
    set(&store, "b", "three").await;
    let second = version_at(&mut versions, 3).await;

    assert_eq!(read(&first, "a").as_deref(), Some("one"));
    assert_eq!(read(&second, "a").as_deref(), Some("two"));
    assert_eq!(read(&second, "b").as_deref(), Some("three"));
    assert_eq!(read(&second, "c"), None);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn published____same_bucket_twice____one_publisher() {
    let store = store(Some(Duration::from_millis(10)));
    let mut generation = store.publishers.generation();
    set(&store, "a", "one").await;

    let mut first = store.published("board").unwrap();
    let mut second = store.published("board").unwrap();
    let a = version_at(&mut first, 1).await;
    let b = version_at(&mut second, 1).await;

    // The generation moves just after the version: wait for it.
    let moved =
        tokio::time::timeout(Duration::from_secs(2), generation.wait_for(|g| *g >= 1)).await;

    assert!(Arc::ptr_eq(&a, &b));
    assert!(moved.is_ok(), "the generation did not move");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn published____bucket_not_published_or_unknown____none() {
    let store = store(None);

    assert!(store.published("board").is_none());
    assert!(store.published("other").is_none());
}
