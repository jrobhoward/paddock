//! Shared keyvalue buckets (ROADMAP H3): named in the runtime's
//! configuration and opened by several apps, read-only or read-write,
//! through `paddock-sdk`'s `kv` wrapper and the SDK fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

use common::{ask, fixture};
use paddock_bundle::manifest::{AccessMode, Capability, Durability};
use paddock_host::{InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig, SharedBucket};

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let bucket = |quota| SharedBucket {
            durability: Durability::Memory,
            quota,
        };
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 16,
            buckets: BTreeMap::from([
                ("board".to_string(), bucket(None)),
                ("small".to_string(), bucket(Some(16))),
            ]),
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load(&fixture("paddock_fixture_sdk")).unwrap();
        (rt, app)
    })
}

/// An instance of `app` with keyvalue granted and these shared buckets
/// listed.
fn up(app: &str, shared_buckets: &[(&str, AccessMode)]) -> InstanceHandle {
    let mut spec = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        granted: [Capability::KeyValue].into(),
        ..InstanceSpec::new(app, "1.0.0")
    };
    spec.services.keyvalue.shared = shared_buckets
        .iter()
        .map(|(name, mode)| ((*name).to_string(), *mode))
        .collect();
    let (rt, loaded) = shared();
    rt.spawn(loaded, spec).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_bucket____written_by_one_app____read_by_another() {
    let writer = up("plc", &[("board", AccessMode::Rw)]);
    let reader = up("hmi", &[("board", AccessMode::Ro)]);

    ask(&writer, "set:board:pump-1:on").await;
    let seen = ask(&reader, "get:board:pump-1").await;

    assert_eq!(seen, "on");
    writer.kill();
    reader.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_bucket____read_only____writes_denied_reads_allowed() {
    let reader = up("hmi-ro", &[("board", AccessMode::Ro)]);

    let set = ask(&reader, "set:board:ro-key:x").await;
    let incr = ask(&reader, "incr:board:ro-count:1").await;
    let delete = ask(&reader, "delete:board:ro-key").await;
    let get = ask(&reader, "get:board:ro-key").await;

    assert_eq!(set, "err:access denied");
    assert_eq!(incr, "err:access denied");
    assert_eq!(delete, "err:access denied");
    assert_eq!(get, "none");
    reader.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_bucket____not_listed_or_not_granted____access_denied() {
    let unlisted = up("other", &[]);
    let (rt, loaded) = shared();
    let mut spec = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        granted: [Capability::KeyValue].into(),
        ..InstanceSpec::new("narrowed", "1.0.0")
    };
    spec.services.keyvalue.shared = BTreeMap::from([("board".to_string(), AccessMode::Rw)]);
    let narrowed = rt
        .spawn(loaded, spec.with_granted_buckets(&BTreeSet::new()))
        .unwrap();

    let not_listed = ask(&unlisted, "get:board:k").await;
    let not_granted = ask(&narrowed, "get:board:k").await;

    assert_eq!(not_listed, "err:access denied");
    assert_eq!(not_granted, "err:access denied");
    unlisted.kill();
    narrowed.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_bucket____not_configured_on_node____no_such_bucket() {
    let a = up("lost", &[("nowhere", AccessMode::Rw)]);

    let result = ask(&a, "get:nowhere:k").await;

    assert_eq!(result, "err:no such bucket");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_bucket____quota____counts_every_app_writing_it() {
    let a = up("quota-a", &[("small", AccessMode::Rw)]);
    let b = up("quota-b", &[("small", AccessMode::Rw)]);

    let first = ask(&a, "set:small:a:12345").await;
    let second = ask(&b, "set:small:b:12345").await;
    let third = ask(&b, "set:small:c:12345").await;

    assert_eq!((first.as_str(), second.as_str()), ("ok", "ok"));
    assert!(third.starts_with("err:storage:"), "{third}");
    a.kill();
    b.kill();
}
