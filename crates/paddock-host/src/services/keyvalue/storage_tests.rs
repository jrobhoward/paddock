#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;

use super::*;

fn run(b: &mut MemBucket, op: Op, quota: Option<u64>) -> Result<Outcome, String> {
    let applied = apply(b, op, quota)?;
    b.bytes = applied.bytes;
    Ok(applied.outcome)
}

fn swap(key: &str, expected: Option<u64>, value: &str) -> Op {
    Op::Swap {
        key: key.into(),
        expected,
        value: value.as_bytes().to_vec(),
    }
}

#[test]
fn apply____changes____listed_once_with_the_revision() {
    let mut b = MemBucket::default();

    let set_many = apply(
        &mut b,
        Op::SetMany(vec![
            ("a".into(), b"1".to_vec()),
            ("b".into(), b"2".to_vec()),
            ("a".into(), b"3".to_vec()),
        ]),
        None,
    )
    .unwrap();
    let missing_delete = apply(&mut b, Op::Delete("zz".into()), None).unwrap();

    assert_eq!(set_many.changed, ["a", "b"]);
    assert_eq!(set_many.revision, Some(1));
    assert!(missing_delete.changed.is_empty());
    assert_eq!(missing_delete.revision, None);
}

#[test]
fn apply____swap____writes_on_matching_revision_only() {
    let mut b = MemBucket::default();

    let created = run(&mut b, swap("k", None, "1"), None).unwrap();
    let exists = run(&mut b, swap("k", None, "2"), None).unwrap();
    let stale = run(&mut b, swap("k", Some(7), "2"), None).unwrap();
    let updated = run(&mut b, swap("k", Some(1), "2"), None).unwrap();
    let delete_stale = run(
        &mut b,
        Op::SwapDelete {
            key: "k".into(),
            expected: 1,
        },
        None,
    )
    .unwrap();

    assert_eq!(created, Outcome::Swapped(1));
    assert_eq!(exists, Outcome::Conflict(Some(1)));
    assert_eq!(stale, Outcome::Conflict(Some(1)));
    assert_eq!(updated, Outcome::Swapped(2));
    assert_eq!(delete_stale, Outcome::Conflict(Some(2)));
    assert_eq!(revision(&mut b, "k"), (2, Some(2)));
}

#[test]
fn apply____since____changed_keys_and_latest_delete() {
    let mut b = MemBucket::default();
    run(&mut b, set("p-1", "a"), None).unwrap();
    run(&mut b, set("p-2", "a"), None).unwrap();
    run(&mut b, set("q-1", "a"), None).unwrap();
    run(&mut b, Op::Delete("q-1".into()), None).unwrap();
    run(&mut b, set("p-1", "b"), None).unwrap();

    let since = run(
        &mut b,
        Op::Since {
            key: "p-".into(),
            prefix: true,
            after: 2,
        },
        None,
    )
    .unwrap();

    assert_eq!(
        since,
        Outcome::Since {
            revision: 5,
            deleted: 4,
            changed: vec![("p-1".into(), 5)],
        }
    );
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

    assert!(result.unwrap_err().to_string().contains("quota"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn KvStore____disk_bucket_without_data_dir____error() {
    let store = KvStore::open(None, BTreeMap::new()).unwrap();

    let result = store
        .run(&handle("app", "d", Durability::Disk), Op::Get("k".into()))
        .await;

    assert!(result.unwrap_err().to_string().contains("not configured"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn KvStore____disk_swap_and_delete_revision____kept_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let b = handle("app", "cas", Durability::Disk);
    {
        let store = KvStore::open(Some(dir.path()), BTreeMap::new()).unwrap();
        store.run(&b, swap("k", None, "1")).await.unwrap();
        store.run(&b, set("gone", "x")).await.unwrap();
        store.run(&b, Op::Delete("gone".into())).await.unwrap();
        let conflict = store.run(&b, swap("k", Some(9), "2")).await.unwrap();
        assert_eq!(conflict, Outcome::Conflict(Some(1)));
    }

    let store = KvStore::open(Some(dir.path()), BTreeMap::new()).unwrap();
    let entry = store.run(&b, Op::Entry("k".into())).await.unwrap();
    let since = store
        .run(
            &b,
            Op::Since {
                key: "k".into(),
                prefix: false,
                after: 0,
            },
        )
        .await
        .unwrap();

    assert_eq!(entry, Outcome::Entry(Some((b"1".to_vec(), 1))));
    assert_eq!(
        since,
        Outcome::Since {
            revision: 3,
            deleted: 3,
            changed: vec![("k".into(), 1)],
        }
    );
}

// --- scans (`paddock:watch`'s `scan`) ----------------------------------------

/// Where a scan test runs: a memory bucket or a disk bucket.
#[derive(Debug, Clone, Copy)]
enum Backend {
    Memory,
    Disk,
}

/// A bucket on `backend` holding `keys` (each value is its key).
struct Filled {
    mem: MemBucket,
    disk: Disk,
    backend: Backend,
}

impl Filled {
    fn new(backend: Backend, keys: &[&str]) -> Self {
        let mut f = Self {
            mem: MemBucket::default(),
            disk: Disk::in_memory(),
            backend,
        };
        let pairs = keys
            .iter()
            .map(|k| ((*k).to_string(), k.as_bytes().to_vec()))
            .collect();
        f.op(Op::SetMany(pairs));
        f
    }

    fn op(&mut self, op: Op) -> Outcome {
        match self.backend {
            Backend::Memory => run(&mut self.mem, op, None).unwrap(),
            Backend::Disk => {
                let key = ("app".to_string(), "b".to_string());
                self.disk.run(&key, op, None).unwrap().outcome
            }
        }
    }

    /// One page: (keys, next).
    fn page(&mut self, query: &ScanQuery) -> (Vec<String>, Option<String>) {
        match self.op(Op::Scan(query.clone())) {
            Outcome::Page { entries, next, .. } => {
                (entries.into_iter().map(|(k, _, _)| k).collect(), next)
            }
            other => panic!("{other:?}"),
        }
    }

    /// Every page from the first, following `next`.
    fn all(&mut self, query: &ScanQuery) -> (Vec<String>, usize) {
        let mut q = query.clone();
        let mut keys = Vec::new();
        let mut pages = 0;
        loop {
            let (found, next) = self.page(&q);
            pages += 1;
            keys.extend(found);
            match next {
                Some(n) => q.cursor = Some(n),
                None => return (keys, pages),
            }
            assert!(pages < 10_000, "no end");
        }
    }
}

fn q(limit: u32) -> ScanQuery {
    ScanQuery {
        limit,
        ..ScanQuery::default()
    }
}

#[rstest::rstest]
#[case::memory(Backend::Memory)]
#[case::disk(Backend::Disk)]
fn scan____paged_both_ways____every_key_once_in_order(#[case] backend: Backend) {
    let keys: Vec<String> = (0..25).map(|i| format!("k{i:02}")).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let mut b = Filled::new(backend, &refs);

    let (up, up_pages) = b.all(&q(10));
    let (down, _) = b.all(&ScanQuery {
        reverse: true,
        ..q(10)
    });

    assert_eq!(up, keys);
    assert_eq!(up_pages, 3);
    let mut reversed = keys.clone();
    reversed.reverse();
    assert_eq!(down, reversed);
}

#[rstest::rstest]
#[case::memory(Backend::Memory)]
#[case::disk(Backend::Disk)]
fn scan____prefix_and_bounds____only_keys_inside_both(#[case] backend: Backend) {
    let mut b = Filled::new(backend, &["a", "b", "b/1", "b/2", "b/3", "b0", "c"]);

    let prefix = b.page(&ScanQuery {
        prefix: Some("b/".into()),
        ..q(0)
    });
    let bounded = b.page(&ScanQuery {
        prefix: Some("b".into()),
        start: Some("b/2".into()),
        end: Some("b0".into()),
        ..q(0)
    });
    let last = b.page(&ScanQuery {
        reverse: true,
        ..q(1)
    });
    let empty = b.page(&ScanQuery {
        start: Some("c".into()),
        end: Some("a".into()),
        ..q(0)
    });

    assert_eq!(
        prefix,
        (vec!["b/1".into(), "b/2".into(), "b/3".into()], None)
    );
    assert_eq!(bounded, (vec!["b/2".into(), "b/3".into()], None));
    assert_eq!(last, (vec!["c".into()], Some("b0".into())));
    assert_eq!(empty, (vec![], None));
}

#[rstest::rstest]
#[case::memory(Backend::Memory)]
#[case::disk(Backend::Disk)]
fn scan____values_and_revisions____as_written(#[case] backend: Backend) {
    let mut b = Filled::new(backend, &["a", "b"]);
    b.op(set("b", "two"));

    let page = b.op(Op::Scan(q(0)));

    assert_eq!(
        page,
        Outcome::Page {
            entries: vec![
                ("a".into(), b"a".to_vec(), 1),
                ("b".into(), b"two".to_vec(), 2)
            ],
            next: None,
            revision: 2,
        }
    );
}

#[rstest::rstest]
#[case::memory(Backend::Memory)]
#[case::disk(Backend::Disk)]
fn scan____values_past_the_page_size____one_entry_per_page_at_least(#[case] backend: Backend) {
    let mut b = Filled::new(backend, &[]);
    let big = vec![7u8; 700 << 10];
    b.op(Op::SetMany(vec![
        ("a".into(), big.clone()),
        ("b".into(), big.clone()),
        ("c".into(), vec![1]),
    ]));

    let first = b.page(&q(0));
    let (all, pages) = b.all(&q(0));

    assert_eq!(first, (vec!["a".into()], Some("b".into())));
    assert_eq!(all, ["a", "b", "c"]);
    assert_eq!(pages, 2);
}

#[test]
fn prefix_end____last_char_raised____past_maximum_and_surrogates() {
    assert_eq!(prefix_end("ab").as_deref(), Some("ac"));
    assert_eq!(prefix_end("a\u{10FFFF}").as_deref(), Some("b"));
    assert_eq!(prefix_end("a\u{D7FF}").as_deref(), Some("a\u{E000}"));
    assert_eq!(prefix_end("\u{10FFFF}"), None);
    assert_eq!(prefix_end(""), None);
}

fn scan_model(keys: &BTreeMap<String, ()>, query: &ScanQuery) -> Vec<String> {
    let mut found: Vec<String> = keys
        .keys()
        .filter(|k| {
            query
                .prefix
                .as_ref()
                .is_none_or(|p| k.starts_with(p.as_str()))
        })
        .filter(|k| query.start.as_ref().is_none_or(|s| *k >= s))
        .filter(|k| query.end.as_ref().is_none_or(|e| *k < e))
        .cloned()
        .collect();
    if query.reverse {
        found.reverse();
    }
    found
}

fn key_strategy() -> impl proptest::strategy::Strategy<Value = String> {
    proptest::collection::vec(
        proptest::sample::select(vec![
            'a',
            'b',
            '/',
            'é',
            '\u{D7FF}',
            '\u{E000}',
            '\u{10FFFF}',
        ]),
        0..4,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Paging through any query on either backend returns exactly the keys
    /// a filter over the sorted keys does.
    #[test]
    fn scan____any_query_paged____same_keys_as_the_model(
        keys in proptest::collection::btree_map(key_strategy(), Just(()), 0..30),
        prefix in proptest::option::of(key_strategy()),
        start in proptest::option::of(key_strategy()),
        end in proptest::option::of(key_strategy()),
        reverse in any::<bool>(),
        limit in 1u32..5,
        disk in any::<bool>(),
    ) {
        let refs: Vec<&str> = keys.keys().map(String::as_str).collect();
        let backend = if disk { Backend::Disk } else { Backend::Memory };
        let mut b = Filled::new(backend, &refs);
        let query = ScanQuery { prefix, start, end, reverse, limit, cursor: None };

        let (found, _) = b.all(&query);

        prop_assert_eq!(found, scan_model(&keys, &query));
    }
}
