#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;

use super::*;

fn run(b: &mut MemBucket, op: Op, quota: Option<u64>) -> Result<Outcome, String> {
    let (outcome, bytes) = apply(b, op, quota)?;
    b.bytes = bytes;
    Ok(outcome)
}

fn set(k: &str, v: &str) -> Op {
    Op::Set(k.into(), v.as_bytes().to_vec())
}

#[test]
fn apply____set_then_get____returns_value() {
    let mut b = MemBucket::default();
    run(&mut b, set("k", "v"), None).unwrap();

    let got = run(&mut b, Op::Get("k".into()), None).unwrap();

    assert_eq!(got, Outcome::Value(Some(b"v".to_vec())));
}

#[test]
fn apply____set_and_delete____bytes_tracked() {
    let mut b = MemBucket::default();

    run(&mut b, set("ab", "123"), None).unwrap();
    let after_set = b.bytes;
    run(&mut b, set("ab", "1"), None).unwrap();
    let after_overwrite = b.bytes;
    run(&mut b, Op::Delete("ab".into()), None).unwrap();

    assert_eq!((after_set, after_overwrite, b.bytes), (5, 3, 0));
}

#[test]
fn apply____set_past_quota____refused_and_unchanged() {
    let mut b = MemBucket::default();
    run(&mut b, set("a", "1234"), Some(8)).unwrap();

    let result = run(&mut b, set("b", "1234"), Some(8));

    assert_eq!(result, Err("bucket quota of 8 bytes exceeded".into()));
    assert_eq!(
        run(&mut b, Op::Get("b".into()), None).unwrap(),
        Outcome::Value(None)
    );
    assert_eq!(b.bytes, 5);
}

#[test]
fn apply____overwrite_within_quota____allowed_even_when_near_full() {
    let mut b = MemBucket::default();
    run(&mut b, set("a", "1234567"), Some(8)).unwrap();

    let result = run(&mut b, set("a", "7654321"), Some(8));

    assert!(result.is_ok());
}

#[test]
fn apply____set_many_past_quota____whole_batch_refused() {
    let mut b = MemBucket::default();
    let pairs = vec![
        ("a".into(), b"12".to_vec()),
        ("b".into(), b"1234567".to_vec()),
    ];

    let result = run(&mut b, Op::SetMany(pairs), Some(8));

    assert!(result.is_err());
    assert_eq!(
        run(&mut b, Op::Get("a".into()), None).unwrap(),
        Outcome::Value(None)
    );
}

#[test]
fn apply____increment____decimal_counter_from_zero() {
    let mut b = MemBucket::default();

    run(&mut b, Op::Increment("c".into(), 5), None).unwrap();
    let second = run(&mut b, Op::Increment("c".into(), 2), None).unwrap();

    assert_eq!(second, Outcome::Count(7));
    assert_eq!(
        run(&mut b, Op::Get("c".into()), None).unwrap(),
        Outcome::Value(Some(b"7".to_vec()))
    );
}

#[test]
fn apply____increment_non_number____error() {
    let mut b = MemBucket::default();
    run(&mut b, set("c", "abc"), None).unwrap();

    let result = run(&mut b, Op::Increment("c".into(), 1), None);

    assert!(result.unwrap_err().contains("not a decimal number"));
}

#[test]
fn apply____increment_overflow____error() {
    let mut b = MemBucket::default();
    run(&mut b, set("c", &u64::MAX.to_string()), None).unwrap();

    let result = run(&mut b, Op::Increment("c".into(), 1), None);

    assert!(result.unwrap_err().contains("overflows"));
}

#[test]
fn apply____keys_across_pages____cursor_walks_every_key_once() {
    let mut b = MemBucket::default();
    for i in 0..(PAGE * 2 + 3) {
        run(&mut b, set(&format!("k{i:04}"), "x"), None).unwrap();
    }

    let mut all = Vec::new();
    let mut cursor = None;
    loop {
        let Outcome::Keys(keys, next) = run(&mut b, Op::Keys(cursor), None).unwrap() else {
            panic!("expected keys");
        };
        all.extend(keys);
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    assert_eq!(all.len(), PAGE * 2 + 3);
    assert!(all.windows(2).all(|w| w[0] < w[1]));
}

fn revision(b: &mut MemBucket, key: &str) -> (u64, Option<u64>) {
    match run(b, Op::Revision(key.into()), None).unwrap() {
        Outcome::Revision { bucket, key } => (bucket, key),
        other => panic!("{other:?}"),
    }
}

#[test]
fn apply____each_change____raises_bucket_revision_and_stamps_keys() {
    let mut b = MemBucket::default();
    let fresh = revision(&mut b, "a");

    run(&mut b, set("a", "1"), None).unwrap();
    run(&mut b, set("b", "1"), None).unwrap();
    run(&mut b, Op::Increment("a".into(), 1), None).unwrap();

    assert_eq!(fresh, (0, None));
    assert_eq!(revision(&mut b, "a"), (3, Some(3)));
    assert_eq!(revision(&mut b, "b"), (3, Some(2)));
}

#[test]
fn apply____batch____one_revision_for_every_key() {
    let mut b = MemBucket::default();
    run(&mut b, set("x", "0"), None).unwrap();

    run(
        &mut b,
        Op::SetMany(vec![
            ("a".into(), b"1".to_vec()),
            ("b".into(), b"2".to_vec()),
        ]),
        None,
    )
    .unwrap();

    assert_eq!(revision(&mut b, "a"), (2, Some(2)));
    assert_eq!(revision(&mut b, "b"), (2, Some(2)));
    assert_eq!(revision(&mut b, "x"), (2, Some(1)));
}

#[test]
fn apply____reads_failures_and_missing_deletes____leave_revision() {
    let mut b = MemBucket::default();
    run(&mut b, set("a", "1234"), Some(8)).unwrap();

    run(&mut b, Op::Get("a".into()), None).unwrap();
    run(&mut b, set("b", "1234"), Some(8)).unwrap_err();
    run(&mut b, Op::Delete("missing".into()), None).unwrap();
    let unchanged = revision(&mut b, "a");
    run(&mut b, Op::Delete("a".into()), None).unwrap();

    assert_eq!(unchanged, (1, Some(1)));
    assert_eq!(revision(&mut b, "a"), (2, None));
}

#[test]
fn apply____get_many____missing_keys_are_none() {
    let mut b = MemBucket::default();
    run(&mut b, set("a", "1"), None).unwrap();

    let got = run(&mut b, Op::GetMany(vec!["a".into(), "z".into()]), None).unwrap();

    assert_eq!(
        got,
        Outcome::Values(vec![Some(("a".into(), b"1".to_vec())), None])
    );
}

fn handle(app: &str, name: &str, durability: Durability) -> BucketHandle {
    BucketHandle {
        space: app.into(),
        name: name.into(),
        durability,
        quota: None,
        writable: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn KvStore____same_bucket_name_in_two_apps____isolated() {
    let store = KvStore::open(None, BTreeMap::new()).unwrap();
    let a = handle("app-a", "data", Durability::Memory);
    let b = handle("app-b", "data", Durability::Memory);

    store.run(&a, set("k", "from-a")).await.unwrap();
    let seen_by_b = store.run(&b, Op::Get("k".into())).await.unwrap();

    assert_eq!(seen_by_b, Outcome::Value(None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn KvStore____disk_bucket____survives_reopening_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let b = handle("app", "durable", Durability::Disk);
    {
        let store = KvStore::open(Some(dir.path()), BTreeMap::new()).unwrap();
        store.run(&b, set("k", "kept")).await.unwrap();
        store.run(&b, set("gone", "x")).await.unwrap();
        store.run(&b, Op::Delete("gone".into())).await.unwrap();
    }

    let store = KvStore::open(Some(dir.path()), BTreeMap::new()).unwrap();
    let kept = store.run(&b, Op::Get("k".into())).await.unwrap();
    let keys = store.run(&b, Op::Keys(None)).await.unwrap();

    assert_eq!(kept, Outcome::Value(Some(b"kept".to_vec())));
    assert_eq!(keys, Outcome::Keys(vec!["k".into()], None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn KvStore____disk_quota_after_reopen____counts_existing_data() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = handle("app", "q", Durability::Disk);
    b.quota = Some(10);
    {
        let store = KvStore::open(Some(dir.path()), BTreeMap::new()).unwrap();
        store.run(&b, set("a", "12345678")).await.unwrap();
    }

    let store = KvStore::open(Some(dir.path()), BTreeMap::new()).unwrap();
    let result = store.run(&b, set("b", "12")).await;

    assert!(result.unwrap_err().contains("quota"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn KvStore____disk_bucket_without_data_dir____error() {
    let store = KvStore::open(None, BTreeMap::new()).unwrap();

    let result = store
        .run(&handle("app", "d", Durability::Disk), Op::Get("k".into()))
        .await;

    assert!(result.unwrap_err().contains("not configured"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn KvStore____disk_revisions____kept_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let b = handle("app", "revs", Durability::Disk);
    {
        let store = KvStore::open(Some(dir.path()), BTreeMap::new()).unwrap();
        store.run(&b, set("a", "1")).await.unwrap();
        store.run(&b, set("b", "1")).await.unwrap();
        store.run(&b, Op::Delete("b".into())).await.unwrap();
    }

    let store = KvStore::open(Some(dir.path()), BTreeMap::new()).unwrap();
    let a = store.run(&b, Op::Revision("a".into())).await.unwrap();
    let gone = store.run(&b, Op::Revision("b".into())).await.unwrap();
    store.run(&b, set("c", "1")).await.unwrap();
    let c = store.run(&b, Op::Revision("c".into())).await.unwrap();

    assert_eq!(
        a,
        Outcome::Revision {
            bucket: 3,
            key: Some(1)
        }
    );
    assert_eq!(
        gone,
        Outcome::Revision {
            bucket: 3,
            key: None
        }
    );
    assert_eq!(
        c,
        Outcome::Revision {
            bucket: 4,
            key: Some(4)
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn KvStore____shared_bucket____one_namespace_for_every_app() {
    let store = KvStore::open(None, BTreeMap::new()).unwrap();
    let shared = handle(SHARED, "points", Durability::Memory);
    let own = handle("app-a", "points", Durability::Memory);

    store.run(&shared, set("p1", "on")).await.unwrap();
    let again = store
        .run(&shared.clone(), Op::Get("p1".into()))
        .await
        .unwrap();
    let app_bucket = store.run(&own, Op::Get("p1".into())).await.unwrap();

    assert_eq!(again, Outcome::Value(Some(b"on".to_vec())));
    assert_eq!(app_bucket, Outcome::Value(None));
}

#[test]
fn KvStore____shared_disk_bucket_without_data_dir____refused() {
    let shared = BTreeMap::from([(
        "points".to_string(),
        SharedBucket {
            durability: Durability::Disk,
            quota: None,
        },
    )]);

    let err = KvStore::open(None, shared).unwrap_err();

    assert!(err.contains("points"), "{err}");
}
