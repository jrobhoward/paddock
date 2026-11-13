//! Sender channels (ROADMAP AP1): the native fixture's threads asking the
//! node through senders of their own, through `bulkhead-runner`. Each
//! `sender` command runs on a thread of the fixture's while its callback
//! thread waits, so every answer here comes during a running callback.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use bulkhead_bundle::manifest::{Bucket, Capability, Durability, NativeProgram};
use bulkhead_host::acl::Acl;
use bulkhead_host::{
    ExitReason, HostObjectRequest, InstanceHandle, InstanceSpec, LoadedApp, NativeCode,
    NativeConfig, Runtime, RuntimeConfig,
};
use common::{ask, ask_until, native_fixtures, text};

fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let mut native = NativeConfig::default();
        native.runner = Some(native_fixtures().0.clone());
        // FreeBSD's runner has no sandbox yet (ROADMAP FB2).
        native.landlock_optional = common::NO_SANDBOX;
        let rt = Runtime::new(RuntimeConfig {
            max_instances: 32,
            native,
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

/// A native instance of `app` with keyvalue (its own `data` bucket) and
/// metrics granted, `senders` its `senders_max`, accepting messages from
/// `accept_from` (every app when empty).
fn up(app: &str, senders: Option<u32>, accept_from: &[&str]) -> InstanceHandle {
    let (rt, loaded) = shared();
    let mut spec = InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        granted: [Capability::KeyValue, Capability::Metrics].into(),
        host_objects: HostObjectRequest {
            senders,
            ..HostObjectRequest::default()
        },
        acl: Acl {
            accept_from: accept_from.iter().map(|a| (*a).to_string()).collect(),
            may_call: Vec::new(),
        },
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
    spec.services.metrics.prefix = "snd".into();
    spec.services.metrics.max_series = 10;
    rt.spawn(loaded, spec).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender____messaging_keyvalue_and_metrics_from_a_thread____answered() {
    let a = up("snd-ops", None, &[]);
    let b = up("snd-ops-peer", None, &[]);
    let b_pid = ask(&b, "self").await;

    let opened = ask(&a, "sender open").await;
    let own = ask(&a, "self").await;
    let from_sender = ask(&a, "sender 0 self").await;
    let set = ask(&a, "sender 0 kv set data s-1 one").await;
    let seen_by_callback = ask(&a, "kv get data s-1").await;
    let got = ask(&a, "sender 0 kv get data s-1").await;
    let counted = ask(&a, "sender 0 kv incr data s-count 3").await;
    let called = ask(&a, &format!("sender 0 call {b_pid} echo hello")).await;
    let cast = ask(&a, &format!("sender 0 cast {b_pid} hi")).await;
    let infos = ask_until(&b, "infos", |i| i.contains("cast hi")).await;
    let metric = ask(&a, "sender 0 metric register snd_events").await;
    let added = ask(&a, &format!("sender 0 metric add {metric} 2")).await;

    assert_eq!(opened, "0");
    assert_eq!(from_sender, own);
    assert_eq!(set, "ok");
    assert_eq!(seen_by_callback, "one");
    assert_eq!(got, "one");
    assert_eq!(counted, "3");
    assert_eq!(called, "hello");
    assert_eq!(cast, "ok");
    assert!(infos.contains("cast hi"), "{infos}");
    assert!(metric.parse::<u32>().is_ok(), "{metric}");
    assert_eq!(added, "ok");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender____watch____access_denied() {
    let a = up("snd-watch", None, &[]);
    ask(&a, "sender open").await;

    let watched = ask(&a, "sender 0 kv watch data w-1").await;

    assert_eq!(watched, "err:AccessDenied");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender____call_the_target_does_not_accept____access_denied_as_the_instance() {
    let a = up("snd-acl", None, &[]);
    let guarded = up("snd-acl-guarded", None, &["snd-acl-friend"]);
    let to = ask(&guarded, "self").await;
    ask(&a, "sender open").await;

    let from_sender = ask(&a, &format!("sender 0 call {to} echo x")).await;
    let from_callback = ask(&a, &format!("call {to} echo x")).await;

    assert_eq!(from_sender, "err:AccessDenied");
    assert_eq!(from_callback, from_sender);
    a.kill();
    guarded.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender____senders_max_reached____limit_reached_until_one_drops() {
    let a = up("snd-max", Some(2), &[]);

    let first = ask(&a, "sender open").await;
    let cloned = ask(&a, "sender clone 0").await;
    let over = ask(&a, "sender open").await;
    let over_by_clone = ask(&a, "sender clone 0").await;
    ask(&a, "sender drop 0").await;
    let after_drop = ask_until(&a, "sender open", |r| !r.starts_with("err:")).await;

    assert_eq!(first, "0");
    assert_eq!(cloned, "1");
    assert_eq!(over, "err:LimitReached");
    assert_eq!(over_by_clone, "err:LimitReached");
    assert_eq!(after_drop, "2");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender____calls_in_flight____each_answered_by_its_id() {
    let a = up("snd-flight", None, &[]);
    let b = up("snd-flight-b", None, &[]);
    let c = up("snd-flight-c", None, &[]);
    let targets = format!("{},{}", ask(&b, "self").await, ask(&c, "self").await);
    ask(&a, "sender open").await;

    let answered = ask(&a, &format!("sender 0 calls {targets} 100")).await;

    assert_eq!(answered, "100/100");
    a.kill();
    b.kill();
    c.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender____callback_running____answered_during_it() {
    let a = up("snd-busy", None, &[]);
    ask(&a, "sender open").await;
    ask(&a, "sender 0 kv set data b-1 x").await;

    let reply = ask(&a, "sender busy 500 0 kv get data b-1").await;

    let (took_us, got) = reply.split_once(' ').unwrap();
    assert_eq!(got, "x");
    assert!(
        took_us.parse::<u64>().unwrap() < 250_000,
        "the get waited for the callback: {took_us} µs"
    );
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender____request_a_sender_may_not_make____instance_crashes() {
    let a = up("snd-bad", None, &[]);
    ask(&a, "sender open").await;

    let _ = a.call(text("sender 0 bad"), Duration::from_secs(20)).await;
    let reason = a.wait().await;

    let ExitReason::Crashed(why) = &reason else {
        panic!("expected a crash, got {reason:?}");
    };
    assert!(why.contains("not allowed on a sender channel"), "{why}");
}
