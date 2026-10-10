//! `paddock:watch` (ROADMAP H4): revisioned reads, compare-and-swap and
//! change watches, through `paddock-sdk`'s `watch` wrapper and the SDK
//! fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use common::{ask, ask_until, fixture};
use paddock_bundle::manifest::{AccessMode, Bucket, Capability, Durability};
use paddock_host::{
    HostObjectRequest, InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig,
    SharedBucket,
};

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 32,
            buckets: BTreeMap::from([(
                "board".to_string(),
                SharedBucket {
                    durability: Durability::Memory,
                    quota: None,
                },
            )]),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load(&fixture("paddock_fixture_sdk")).unwrap();
        (rt, app)
    })
}

/// An instance of `app` with keyvalue granted, its own `data` bucket and
/// the shared `board` bucket in `mode`. Tests share the runtime, so each
/// uses its own keys.
fn spec(app: &str, mode: AccessMode) -> InstanceSpec {
    let mut spec = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        granted: [Capability::KeyValue].into(),
        ..InstanceSpec::new(app, "1.0.0")
    };
    spec.services.keyvalue.buckets = BTreeMap::from([(
        "data".to_string(),
        Bucket {
            durability: Durability::Memory,
            quota: None,
        },
    )]);
    spec.services.keyvalue.shared = BTreeMap::from([("board".to_string(), mode)]);
    spec
}

fn up(app: &str, mode: AccessMode) -> InstanceHandle {
    let (rt, loaded) = shared();
    rt.spawn(loaded, spec(app, mode)).unwrap()
}

/// The revision in a `<id>@<revision>` or `<value>@<revision>` reply.
fn rev(reply: &str) -> u64 {
    reply.rsplit('@').next().unwrap().parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch____key_written_by_another_app____notice_with_its_revision() {
    let watcher = up("w-key-watcher", AccessMode::Ro);
    let writer = up("w-key-writer", AccessMode::Rw);
    ask(&watcher, "watch:board:w-key").await;

    ask(&writer, "set:board:w-key:on").await;
    ask(&writer, "set:board:w-key-other:on").await;
    let entry = ask(&watcher, "entry:board:w-key").await;

    let expected = format!("w-key@{}", rev(&entry));
    assert_eq!(
        ask_until(&watcher, "changes", |c| !c.is_empty()).await,
        expected
    );
    assert!(entry.starts_with("on@"), "{entry}");
    watcher.kill();
    writer.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch____many_writes_while_busy____one_notice_with_the_latest_revision() {
    let watcher = up("w-conflate-watcher", AccessMode::Ro);
    let writer = up("w-conflate-writer", AccessMode::Rw);
    ask(&watcher, "watch:board:w-conflate").await;
    let busy = {
        let w = watcher.clone();
        tokio::spawn(async move { ask(&w, "busy:500").await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    for i in 0..20 {
        ask(&writer, &format!("set:board:w-conflate:{i}")).await;
    }
    busy.await.unwrap();
    let latest = rev(&ask(&writer, "entry:board:w-conflate").await);
    let changes = ask_until(&watcher, "changes", |c| !c.is_empty()).await;

    assert_eq!(changes, format!("w-conflate@{latest}"));
    watcher.kill();
    writer.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_prefix____keys_written____notices_for_matching_keys_only() {
    let a = up("w-prefix", AccessMode::Rw);
    ask(&a, "watch-prefix:data:pump-").await;

    ask(&a, "set:data:pump-1:on").await;
    ask(&a, "set:data:valve-1:open").await;
    ask(&a, "set:data:pump-2:off").await;
    let changes = ask_until(&a, "changes", |c| c.split(',').count() >= 2).await;

    let keys: Vec<&str> = changes
        .split(',')
        .map(|c| c.split('@').next().unwrap())
        .collect();
    assert_eq!(keys, ["pump-1", "pump-2"]);
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume____after_revision____replays_changes_or_too_old_after_delete() {
    let a = up("w-resume", AccessMode::Rw);
    ask(&a, "set:data:r-1:a").await;
    let from = rev(&ask(&a, "rev:data").await);
    ask(&a, "set:data:r-2:b").await;
    ask(&a, "set:data:other:c").await;

    let resumed = ask(&a, &format!("resume-prefix:data:r-:{from}")).await;
    let replayed = ask_until(&a, "changes", |c| !c.is_empty()).await;
    ask(&a, "delete:data:r-1").await;
    let too_old = ask(&a, &format!("resume-prefix:data:r-:{from}")).await;
    let now = rev(&ask(&a, "rev:data").await);

    assert_eq!(rev(&resumed), from + 2);
    assert_eq!(replayed, format!("r-2@{}", from + 1));
    assert_eq!(too_old, format!("err:revision too old (bucket at {now})"));
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compare_and_swap____expected_revisions____write_or_conflict() {
    let a = up("w-cas", AccessMode::Rw);

    let created = ask(&a, "cas:data:c-1:-:one").await;
    let again = ask(&a, "cas:data:c-1:-:two").await;
    let stale = ask(&a, "cas:data:c-1:0:two").await;
    let swapped = ask(&a, &format!("cas:data:c-1:{created}:two")).await;
    let entry = ask(&a, "entry:data:c-1").await;
    let bad_delete = ask(&a, &format!("cad:data:c-1:{created}")).await;
    let deleted = ask(&a, &format!("cad:data:c-1:{swapped}")).await;

    let created_rev: u64 = created.parse().unwrap();
    assert_eq!(again, format!("err:conflict (key at {created_rev})"));
    assert_eq!(stale, format!("err:conflict (key at {created_rev})"));
    assert_eq!(entry, format!("two@{swapped}"));
    assert_eq!(bad_delete, format!("err:conflict (key at {swapped})"));
    assert!(deleted.parse::<u64>().unwrap() > swapped.parse::<u64>().unwrap());
    assert_eq!(ask(&a, "entry:data:c-1").await, "none");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_bucket____read_only_grant____watch_and_read_allowed_writes_denied() {
    let a = up("w-ro", AccessMode::Ro);

    let watch = ask(&a, "watch:board:w-ro").await;
    let read = ask(&a, "entry:board:w-ro").await;
    let cas = ask(&a, "cas:board:w-ro:-:x").await;

    assert!(watch.contains('@'), "{watch}");
    assert_eq!(read, "none");
    assert_eq!(cas, "err:access denied");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch____not_granted_or_unlisted____access_denied() {
    let (rt, loaded) = shared();
    let mut ungranted = spec("w-denied", AccessMode::Rw);
    ungranted.granted.clear();
    let a = rt.spawn(loaded, ungranted).unwrap();
    let b = up("w-unlisted", AccessMode::Rw);

    let not_granted = ask(&a, "watch:data:k").await;
    let unlisted = ask(&b, "watch:elsewhere:k").await;

    assert_eq!(not_granted, "err:access denied");
    assert_eq!(unlisted, "err:access denied");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch____over_watches_max____limit_reached_until_unwatch() {
    let (rt, loaded) = shared();
    let mut limited = spec("w-limit", AccessMode::Rw);
    limited.host_objects = HostObjectRequest {
        watches: Some(1),
        ..HostObjectRequest::default()
    };
    let a = rt.spawn(loaded, limited).unwrap();
    let first = ask(&a, "watch:data:l-1").await;

    let second = ask(&a, "watch:data:l-2").await;
    let id = first.split('@').next().unwrap();
    ask(&a, &format!("unwatch:{id}")).await;
    let after = ask(&a, "watch:data:l-2").await;

    assert_eq!(second, "err:watch limit reached");
    assert!(after.contains('@'), "{after}");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch____watcher_exits____writes_continue() {
    let writer = up("w-exit-writer", AccessMode::Rw);
    let (rt, loaded) = shared();
    let mut limited = spec("w-exit", AccessMode::Ro);
    limited.host_objects = HostObjectRequest {
        watches: Some(1),
        ..HostObjectRequest::default()
    };
    let watcher = rt.spawn(loaded, limited).unwrap();
    ask(&watcher, "watch:board:w-exit").await;

    watcher.kill();
    watcher.wait().await;
    let write = ask(&writer, "set:board:w-exit:x").await;

    assert_eq!(write, "ok");
    writer.kill();
}
