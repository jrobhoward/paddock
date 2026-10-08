//! `paddock-sdk`'s `config` and `kv` wrappers, through the SDK fixture,
//! against the host's config and keyvalue services.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use common::{ask, fixture};
use paddock_bundle::manifest::{Bucket, ByteSize, Capability, Durability};
use paddock_host::{InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 16,
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load(&fixture("paddock_fixture_sdk")).unwrap();
        (rt, app)
    })
}

fn spec(app: &str, caps: &[Capability]) -> InstanceSpec {
    let mut s = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        granted: caps.iter().copied().collect(),
        ..InstanceSpec::new(app, "1.0.0")
    };
    s.services.config.values = BTreeMap::from([
        ("db.host".to_string(), "db.internal".to_string()),
        ("mode".to_string(), "fast".to_string()),
    ]);
    s.services.keyvalue.buckets = BTreeMap::from([
        (
            "data".to_string(),
            Bucket {
                durability: Durability::Memory,
                quota: None,
            },
        ),
        (
            "tiny".to_string(),
            Bucket {
                durability: Durability::Memory,
                quota: Some(ByteSize(8)),
            },
        ),
    ]);
    s
}

fn up(app: &str, caps: &[Capability]) -> InstanceHandle {
    let (rt, loaded) = shared();
    rt.spawn(loaded, spec(app, caps)).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config____granted____get_and_get_all() {
    let h = up("sdk-config", &[Capability::Config]);

    let one = ask(&h, "config:db.host").await;
    let missing = ask(&h, "config:nope").await;
    let all = ask(&h, "config-all").await;

    assert_eq!(one, "db.internal");
    assert_eq!(missing, "none");
    assert_eq!(all, "db.host=db.internal,mode=fast");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_and_kv____not_granted____denied() {
    let h = up("sdk-denied", &[]);

    let config = ask(&h, "config:db.host").await;
    let kv = ask(&h, "get:data:k").await;

    assert_eq!(config, "err:access denied");
    assert_eq!(kv, "err:access denied");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv____store_operations____round_trip() {
    let h = up("sdk-kv", &[Capability::KeyValue]);

    let set = ask(&h, "set:data:greeting:hello").await;
    let got = ask(&h, "get:data:greeting").await;
    let exists = ask(&h, "exists:data:greeting").await;
    ask(&h, "delete:data:greeting").await;
    let gone = ask(&h, "get:data:greeting").await;
    let count = [
        ask(&h, "incr:data:hits:2").await,
        ask(&h, "incr:data:hits:3").await,
    ];
    let unlisted = ask(&h, "get:elsewhere:k").await;

    assert_eq!(set, "ok");
    assert_eq!(got, "hello");
    assert_eq!(exists, "true");
    assert_eq!(gone, "none");
    assert_eq!(count, ["2".to_string(), "5".to_string()]);
    assert_eq!(unlisted, "err:access denied");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv____batch_and_paging____keys_spans_pages() {
    // More keys than one `list-keys` page (256), so `keys` must follow the
    // cursor.
    let h = up("sdk-batch", &[Capability::KeyValue]);

    let filled = ask(&h, "fill:data:600").await;
    let keys = ask(&h, "keys:data").await;
    let many = ask(&h, "many:data:k0000,nope,k0599").await;
    ask(&h, "drop-many:data:k0000,k0001").await;
    let after = ask(&h, "keys:data").await;

    assert_eq!(filled, "ok");
    let keys: Vec<&str> = keys.split(',').collect();
    assert_eq!(keys.len(), 600);
    assert_eq!((keys[0], keys[599]), ("k0000", "k0599"));
    assert_eq!(many, "v,-,v");
    assert_eq!(after.split(',').count(), 598);
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv____typed_values_and_quota____cbor_and_errors() {
    let h = up("sdk-typed", &[Capability::KeyValue]);

    let point = ask(&h, "point:data:p:3:-4").await;
    ask(&h, "set:data:raw:not-cbor").await;
    let bad = ask(&h, "point-bytes:data:raw").await;
    let over = ask(&h, "set:tiny:key:far-too-long").await;

    assert_eq!(point, "3,-4");
    assert!(bad.starts_with("err:value encoding:"), "{bad}");
    assert!(over.starts_with("err:storage:"), "{over}");
    h.kill();
}
