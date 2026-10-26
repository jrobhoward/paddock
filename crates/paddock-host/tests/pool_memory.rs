//! Releasing free Store slots' kept pages: on request
//! (`Runtime::release_idle_memory`), and once instance starts have been
//! quiet for `release_idle_after`; starts during a release wait for it
//! instead of finding the pool full.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{ask, fixture, spec};
use paddock_host::{LoadedApp, Runtime, RuntimeConfig};

fn runtime(release_idle_after: Option<Duration>) -> (Runtime, LoadedApp) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 4,
        release_idle_after,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&fixture("paddock_fixture_chaos")).unwrap();
    (rt, app)
}

/// The value of metric `name` (no labels) in the runtime's metrics.
fn metric(rt: &Runtime, name: &str) -> u64 {
    rt.render_metrics()
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
        .unwrap_or_else(|| panic!("no {name}"))
}

/// Start an instance, have it write `mib` MiB, and let it exit: its slot
/// keeps the pages.
async fn burst(rt: &Runtime, app: &LoadedApp, mib: u32) {
    let h = rt.spawn(app, spec("")).unwrap();
    assert_eq!(ask(&h, &format!("alloc:{mib}")).await, "ok");
    h.kill();
    h.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_idle_memory____slot_freed_after_a_burst____pages_given_back_and_slot_reused() {
    let (rt, app) = runtime(None);
    burst(&rt, &app, 8).await;
    let kept = metric(&rt, "paddock_pool_resident_bytes");

    let released = rt.release_idle_memory().await;
    let after = metric(&rt, "paddock_pool_resident_bytes");
    let total = metric(&rt, "paddock_pool_released_bytes_total");
    let h = rt.spawn(&app, spec("")).unwrap();
    let reused = ask(&h, "echo:up").await;

    assert!(kept >= 8 << 20, "the freed slot kept its pages: {kept}");
    assert_eq!(released, kept);
    assert_eq!(after, 0);
    assert_eq!(total, released);
    assert_eq!(reused, "up");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_idle_after____starts_quiet____released_on_its_own() {
    let (rt, app) = runtime(Some(Duration::from_millis(200)));
    burst(&rt, &app, 4).await;

    let mut resident = metric(&rt, "paddock_pool_resident_bytes");
    for _ in 0..100 {
        if resident == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        resident = metric(&rt, "paddock_pool_resident_bytes");
    }

    assert_eq!(resident, 0, "released within 5 s of a 200 ms quiet period");
    assert!(metric(&rt, "paddock_pool_released_bytes_total") >= 4 << 20);
}

/// Starts racing a release wait for it and then succeed, rather than hang.
/// Whether the pool would refuse them without the release holding the free
/// slot permits depends on every free memory slot being warm, which this
/// test does not arrange: the pool has two memory slots per instance, so a
/// cold one is free here either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_idle_memory____starts_racing_it____every_start_succeeds() {
    let (rt, app) = runtime(None);
    for _ in 0..20 {
        burst(&rt, &app, 2).await;
        let starts = async {
            let mut handles = Vec::new();
            for _ in 0..3 {
                let h = rt.spawn(&app, spec("")).unwrap();
                assert_eq!(ask(&h, "echo:up").await, "up");
                handles.push(h);
            }
            handles
        };
        let (_, handles) = tokio::join!(rt.release_idle_memory(), starts);
        for h in handles {
            h.kill();
            h.wait().await;
        }
    }
}
