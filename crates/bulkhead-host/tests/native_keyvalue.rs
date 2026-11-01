//! Keyvalue for native-process apps (`bulkhead_native::app::Bucket`):
//! the fixture executable's `kv` commands through `bulkhead-runner`, on
//! the terms a component gets.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use bulkhead_bundle::manifest::{AccessMode, Bucket, Capability, Durability, NativeProgram};
use bulkhead_host::{
    HostObjectRequest, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime,
    RuntimeConfig, SharedBucket,
};
use common::{ask, ask_until, native_fixtures};

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let mut native = NativeConfig::default();
        native.runner = Some(native_fixtures().0.clone());
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 32,
            native,
            buckets: BTreeMap::from([
                (
                    "board".to_string(),
                    SharedBucket {
                        durability: Durability::Memory,
                        quota: None,
                        read_concurrency: false,
                        publish: None,
                    },
                ),
                (
                    "published".to_string(),
                    SharedBucket {
                        durability: Durability::Memory,
                        quota: None,
                        read_concurrency: false,
                        publish: Some(Duration::from_millis(10)),
                    },
                ),
            ]),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load_native(NativeCode {
            path: native_fixtures().1.clone(),
            kind: NativeProgram::Executable,
        });
        (rt, app)
    })
}

/// A native instance of `app` with keyvalue granted, its own `data`
/// bucket, the shared `board` and `published` (published every 10 ms)
/// buckets in `mode`, and the shared `missing` bucket the node does not
/// configure. Tests share the runtime, so each uses its own keys.
fn spec(app: &str, mode: AccessMode) -> InstanceSpec {
    let mut spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        granted: [Capability::KeyValue].into(),
        ..InstanceSpec::new(app, "1.0.0")
    };
    spec.services.keyvalue.buckets = BTreeMap::from([(
        "data".to_string(),
        Bucket {
            durability: Durability::Memory,
            quota: None,
            read_concurrency: false,
        },
    )]);
    spec.services.keyvalue.shared = BTreeMap::from([
        ("board".to_string(), mode),
        ("published".to_string(), mode),
        ("missing".to_string(), AccessMode::Rw),
    ]);
    spec
}

fn up(app: &str, mode: AccessMode) -> InstanceHandle {
    let (rt, loaded) = shared();
    rt.spawn(loaded, spec(app, mode)).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_kv____operations____answered_as_for_a_component() {
    let a = up("nk-ops", AccessMode::Rw);

    let set = ask(&a, "kv set data o-1 one").await;
    let got = ask(&a, "kv get data o-1").await;
    let missing = ask(&a, "kv get data o-none").await;
    ask(&a, "kv setmany data o-2=two,o-3=three").await;
    let many = ask(&a, "kv many data o-1,o-none,o-3").await;
    let entry = ask(&a, "kv entry data o-2").await;
    let revision: u64 = entry.split('@').nth(1).unwrap().parse().unwrap();
    let scan = ask(&a, "kv scan data o-").await;
    let swapped = ask(&a, &format!("kv cas data o-2 {revision} TWO")).await;
    let conflict = ask(&a, &format!("kv cas data o-2 {revision} again")).await;
    let counted = ask(&a, "kv incr data o-count 5").await;
    ask(&a, "kv del data o-1").await;
    let deleted = ask(&a, "kv get data o-1").await;

    assert_eq!(set, "ok");
    assert_eq!(got, "one");
    assert_eq!(missing, "none");
    assert_eq!(many, "one,-,three");
    assert!(entry.starts_with("two@"), "{entry}");
    assert_eq!(
        scan,
        format!(
            "o-1=one@{},o-2=two@{revision},o-3=three@{revision}",
            revision - 1
        )
    );
    assert_eq!(swapped, (revision + 1).to_string());
    assert!(conflict.starts_with("err:Conflict(Some("), "{conflict}");
    assert_eq!(counted, "5");
    assert_eq!(deleted, "none");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_kv____watched_prefix_written_and_deleted____notices_carry_the_value() {
    let watcher = up("nk-watcher", AccessMode::Ro);
    let writer = up("nk-writer", AccessMode::Rw);
    let id = ask(&watcher, "kv watchp board n-").await;

    ask(&writer, "kv set board n-1 hello").await;
    let written = ask_until(&watcher, "infos", |i| i.contains("n-1 hello@")).await;
    ask(&writer, "kv set board other x").await;
    ask(&writer, "kv del board n-1").await;
    let deleted = ask_until(&watcher, "infos", |i| i.contains("n-1 deleted")).await;

    assert!(
        written.contains(&format!("kv {id} board n-1 hello@")),
        "{written}"
    );
    assert!(
        deleted.contains(&format!("kv {id} board n-1 deleted")),
        "{deleted}"
    );
    assert!(!deleted.contains("other"), "{deleted}");
    watcher.kill();
    writer.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_kv____not_granted_unlisted_read_only_or_unconfigured____refused() {
    let (rt, loaded) = shared();
    let mut ungranted = spec("nk-denied", AccessMode::Rw);
    ungranted.granted.clear();
    let a = rt.spawn(loaded, ungranted).unwrap();
    let b = up("nk-ro", AccessMode::Ro);

    let not_granted = ask(&a, "kv get data k").await;
    let unlisted = ask(&b, "kv get elsewhere k").await;
    let read_only = ask(&b, "kv set board nk-ro x").await;
    let read_only_cas = ask(&b, "kv cas board nk-ro none x").await;
    let reads = ask(&b, "kv get board nk-ro").await;
    let unconfigured = ask(&b, "kv get missing k").await;

    assert_eq!(not_granted, "err:AccessDenied");
    assert_eq!(unlisted, "err:AccessDenied");
    assert_eq!(read_only, "err:AccessDenied");
    assert_eq!(read_only_cas, "err:AccessDenied");
    assert_eq!(reads, "none");
    assert_eq!(unconfigured, "err:NoSuchBucket");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_kv____over_watches_max____limit_reached_until_unwatch() {
    let (rt, loaded) = shared();
    let mut limited = spec("nk-limit", AccessMode::Rw);
    limited.host_objects = HostObjectRequest {
        watches: Some(1),
        ..HostObjectRequest::default()
    };
    let a = rt.spawn(loaded, limited).unwrap();
    let first = ask(&a, "kv watch data l-1").await;

    let second = ask(&a, "kv watch data l-2").await;
    ask(&a, &format!("kv unwatch {first}")).await;
    let after = ask(&a, "kv watch data l-2").await;

    assert_eq!(second, "err:LimitReached");
    assert!(after.parse::<u64>().is_ok(), "{after}");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_kv____watcher_exits____writes_continue() {
    let writer = up("nk-exit-writer", AccessMode::Rw);
    let watcher = up("nk-exit", AccessMode::Ro);
    ask(&watcher, "kv watch board nk-exit").await;

    watcher.kill();
    watcher.wait().await;
    let write = ask(&writer, "kv set board nk-exit x").await;

    assert_eq!(write, "ok");
    writer.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_kv____published_bucket____read_from_the_snapshot_after_a_write() {
    let writer = up("np-writer", AccessMode::Rw);
    let reader = up("np-reader", AccessMode::Ro);
    ask(&writer, "kv set published np-a one").await;
    ask(&writer, "kv set published np-b two").await;

    let got = ask_until(&reader, "kv get published np-a", |r| r == "one").await;
    let mapped = ask(&reader, "published published").await;
    let many = ask(&reader, "kv many published np-a,np-none,np-b").await;
    let missing = ask(&reader, "kv get published np-none").await;

    assert_eq!(got, "one");
    assert!(mapped.parse::<u64>().is_ok(), "{mapped}");
    assert_eq!(
        many,
        ask(&writer, "kv many published np-a,np-none,np-b").await
    );
    assert_eq!(missing, "none");
    writer.kill();
    reader.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_kv____published_bucket_written____own_write_read_at_once() {
    let writer = up("np-own", AccessMode::Rw);
    ask_until(&writer, "published published", |r| r != "none").await;

    ask(&writer, "kv set published np-own v1").await;
    let first = ask(&writer, "kv get published np-own").await;
    ask(&writer, "kv set published np-own v2").await;
    let second = ask(&writer, "kv get published np-own").await;

    assert_eq!((first.as_str(), second.as_str()), ("v1", "v2"));
    writer.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_kv____published_bucket_not_listed____no_snapshot() {
    let (rt, loaded) = shared();
    let mut unlisted = spec("np-unlisted", AccessMode::Ro);
    unlisted.services.keyvalue.shared.remove("published");
    let h = rt.spawn(loaded, unlisted).unwrap();

    let mapped = ask(&h, "published published").await;
    let read = ask(&h, "kv get published np-a").await;

    assert_eq!(mapped, "none");
    assert!(read.starts_with("err:"), "{read}");
    h.kill();
}
