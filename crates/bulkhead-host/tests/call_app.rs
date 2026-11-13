//! Calls to an app by name (ROADMAP DA1): the node picks the instance by
//! the app's routing, as its HTTP ingress does, and checks the messaging
//! ACLs as for a call to a pid. Native fixture instances, through
//! `bulkhead-runner`.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::{
    AppDeployment, Apps, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, Runtime,
    RuntimeConfig,
};
use common::{app_manifest, ask, native_config, native_fixtures};
use serde_json::json;

struct Node {
    rt: Runtime,
    code: LoadedApp,
    /// `shop`: three instances routed by `hash:key`; `guarded`: one,
    /// accepting messages from `friend` only.
    _apps: Apps,
}

fn manifest(instances: serde_json::Value, acl: serde_json::Value) -> serde_json::Value {
    json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": instances,
        "messaging_acl": acl,
        "limits": { "callback_deadline_ms": 5000, "memory_max": "64MiB" }
    })
}

/// A node of its own for each test: an app set's supervisors run on the
/// test's tokio runtime, which ends with the test.
async fn node() -> Node {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native: native_config(),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let code = rt.load_native(NativeCode {
        path: native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    });
    let apps = rt
        .start_apps(vec![
            AppDeployment::new(
                app_manifest(
                    "shop",
                    manifest(json!({ "count": 3, "routing": "hash:key" }), json!({})),
                ),
                code.clone(),
            ),
            AppDeployment::new(
                app_manifest(
                    "guarded",
                    manifest(json!({ "count": 1 }), json!({ "accept_from": ["friend"] })),
                ),
                code.clone(),
            ),
        ])
        .await
        .unwrap();
    Node {
        rt,
        code,
        _apps: apps,
    }
}

/// A native fixture instance outside any app set, as app `app`.
fn caller(n: &Node, app: &str) -> InstanceHandle {
    let spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        ..InstanceSpec::new(app, "1.0.0")
    };
    n.rt.spawn(&n.code, spec).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_app____hash_routed_pool____same_key_same_instance() {
    let n = node().await;
    let c = caller(&n, "ca-hash");

    let mut same = BTreeSet::new();
    for _ in 0..5 {
        same.insert(ask(&c, "call-app shop k-1 self").await);
    }
    let mut spread = BTreeSet::new();
    for i in 0..30 {
        spread.insert(ask(&c, &format!("call-app shop k-{i} self")).await);
    }

    assert_eq!(same.len(), 1, "{same:?}");
    assert!(!same.iter().next().unwrap().starts_with("err:"), "{same:?}");
    assert!(spread.len() > 1, "{spread:?}");
    c.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_app____app_not_running_here____noproc() {
    let n = node().await;
    let c = caller(&n, "ca-none");

    let reply = ask(&c, "call-app nowhere k self").await;

    assert_eq!(reply, "err:Noproc");
    c.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_app____target_does_not_accept_the_caller____access_denied() {
    let n = node().await;
    let stranger = caller(&n, "ca-stranger");
    let friend = caller(&n, "friend");

    let refused = ask(&stranger, "call-app guarded k echo hi").await;
    let allowed = ask(&friend, "call-app guarded k echo hi").await;

    assert_eq!(refused, "err:AccessDenied");
    assert_eq!(allowed, "hi");
    stranger.kill();
    friend.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_app____from_a_sender_with_calls_in_flight____each_answered_by_its_id() {
    let n = node().await;
    let c = caller(&n, "ca-sender");
    ask(&c, "sender open").await;

    let one = ask(&c, "sender 0 call-app shop k-1 echo one").await;
    let many = ask(&c, "sender 0 calls-app shop 64").await;

    assert_eq!(one, "one");
    assert_eq!(many, "64/64");
    c.kill();
}
