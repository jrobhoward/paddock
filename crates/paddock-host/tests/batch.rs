//! `paddock-sdk`'s batches between two apps of the SDK
//! fixture: full batches, one cast per point, and a partial batch sent by
//! its timer.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{app_manifest, ask, ask_until, fixture};
use paddock_host::{AppDeployment, Apps, InstanceHandle, Runtime, RuntimeConfig};

async fn pair(name: &str) -> (Apps, InstanceHandle, InstanceHandle) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 8,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let loaded = rt.load(&fixture("paddock_fixture_sdk")).unwrap();
    let spec =
        serde_json::json!({ "limits": { "mailbox_max": 100000, "callback_deadline_ms": 5000 } });
    let sink = format!("{name}-sink");
    let apps = rt
        .start_apps(vec![
            AppDeployment::new(app_manifest(&sink, spec.clone()), loaded.clone()),
            AppDeployment::new(app_manifest(&format!("{name}-source"), spec), loaded),
        ])
        .await
        .unwrap();
    let sink_h = apps.app(&sink).unwrap().instances()[0].clone();
    let source = apps.app(&format!("{name}-source")).unwrap().instances()[0].clone();
    (apps, source, sink_h)
}

#[rstest::rstest]
#[case::single_casts(0)]
#[case::batches_of_one(1)]
#[case::batches(100)]
#[case::uneven(333)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn produce____batch_size____every_point_arrives(#[case] batch: usize) {
    let (apps, source, sink) = pair(&format!("b{batch}")).await;

    let sent = ask(&source, &format!("produce:b{batch}-sink:1000:{batch}")).await;
    let got = ask_until(&sink, "points", |p| p == "1000").await;

    assert_eq!(sent, "ok");
    assert_eq!(got, "1000");
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn produce_timed____partial_batch____sent_when_its_timer_fires() {
    let (apps, source, sink) = pair("timed").await;

    let started = Instant::now();
    let sent = ask(&source, "produce-timed:timed-sink:250:100:200").await;
    let full = ask_until(&sink, "points", |p| p == "200").await;
    let early = ask(&sink, "points").await;
    let late = ask_until(&sink, "points", |p| p == "250").await;

    assert_eq!(sent, "ok");
    assert_eq!(full, "200", "two full batches at once");
    assert_eq!(early, "200", "the last 50 wait for the timer");
    assert_eq!(late, "250");
    assert!(started.elapsed() >= Duration::from_millis(200));
    apps.stop().await;
}
