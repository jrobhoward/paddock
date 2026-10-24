//! Metrics: apps' own through `paddock:metrics` and the SDK
//! fixture, and the host's per-app counters, rendered as Prometheus text.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::{ask, fixture};
use paddock_bundle::manifest::Capability;
use paddock_host::{InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

fn runtime() -> (Runtime, LoadedApp) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 8,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&fixture("paddock_fixture_sdk")).unwrap();
    (rt, app)
}

fn up(rt: &Runtime, app: &LoadedApp, name: &str, max_series: u32, granted: bool) -> InstanceHandle {
    let mut spec = InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        ..InstanceSpec::new(name, "1.0.0")
    };
    if granted {
        spec.granted = BTreeSet::from([Capability::Metrics]);
    }
    spec.services.metrics.prefix = "shop".into();
    spec.services.metrics.max_series = max_series;
    rt.spawn(app, spec).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_metrics____two_instances_of_one_app____one_series_each() {
    let (rt, app) = runtime();
    let a = up(&rt, &app, "shop", 100, true);
    let b = up(&rt, &app, "shop", 100, true);

    ask(&a, "counter:orders_total:eu:2").await;
    ask(&b, "counter:orders_total:eu:3").await;
    ask(&a, "gauge:queue:eu:7").await;
    ask(&b, "observe:latency:eu:5").await;
    let text = rt.render_metrics();

    assert!(
        text.contains(r#"shop_orders_total{app="shop",l="eu"} 5"#),
        "{text}"
    );
    assert!(
        text.contains(r#"shop_queue{app="shop",l="eu"} 7"#),
        "{text}"
    );
    assert!(
        text.contains(r#"shop_latency_bucket{app="shop",l="eu",le="10"} 1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"shop_latency_bucket{app="shop",l="eu",le="1"} 0"#),
        "{text}"
    );
    assert!(
        text.contains(r#"paddock_instances{app="shop"} 2"#),
        "{text}"
    );
    assert!(
        text.contains(r#"paddock_callbacks_total{app="shop",kind="call"}"#),
        "{text}"
    );
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_metrics____limit_kind_and_grant____refused() {
    let (rt, app) = runtime();
    let limited = up(&rt, &app, "tiny", 1, true);
    let denied = up(&rt, &app, "denied", 100, false);

    let first = ask(&limited, "counter:c:a:1").await;
    let again = ask(&limited, "counter:c:a:1").await;
    let over = ask(&limited, "counter:c:b:1").await;
    let other_kind = ask(&limited, "gauge:c:a:1").await;
    let bad_name = ask(&limited, "counter:bad-name:a:1").await;
    let not_granted = ask(&denied, "counter:c:a:1").await;

    assert_eq!((first.as_str(), again.as_str()), ("ok", "ok"));
    assert_eq!(over, "err:series limit reached");
    assert!(other_kind.starts_with("err:invalid"), "{other_kind}");
    assert!(bad_name.starts_with("err:invalid"), "{bad_name}");
    assert_eq!(not_granted, "err:access denied");
    limited.kill();
    denied.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_metrics____exits_and_callbacks____counted_per_app() {
    let (rt, app) = runtime();
    let a = up(&rt, &app, "counted", 100, true);
    for _ in 0..3 {
        ask(&a, "rev:nothing").await;
    }

    a.kill();
    a.wait().await;
    let text = rt.render_metrics();

    assert!(
        text.contains(r#"paddock_callbacks_total{app="counted",kind="call"} 3"#),
        "{text}"
    );
    assert!(
        text.contains(r#"paddock_instance_exits_total{app="counted",reason="killed"} 1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"paddock_callback_guest_seconds_count{app="counted"} 3"#),
        "{text}"
    );
}
