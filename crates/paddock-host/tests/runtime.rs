//! Runtime configuration and limits.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use common::{LONG, body, fixture, spec, text};
use paddock_host::{HostError, MemoryProfile, Runtime, RuntimeConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn____instance_limit_reached____refused_until_a_slot_frees() {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 1,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&fixture("paddock_fixture_chaos")).unwrap();
    let first = rt.spawn(&app, spec("")).unwrap();

    let refused = rt.spawn(&app, spec(""));
    first.kill();
    first.wait().await;
    let second = rt.spawn(&app, spec(""));

    assert!(
        matches!(refused, Err(HostError::InstanceLimit(1))),
        "{refused:?}"
    );
    assert!(second.is_ok());
    second.unwrap().kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn____memory_max_above_dense_profile____refused() {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 4,
        memory_profile: MemoryProfile::Dense,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&fixture("paddock_fixture_chaos")).unwrap();
    let mut big = spec("");
    big.memory_max = 256 << 20;

    let result = rt.spawn(&app, big);

    assert!(
        matches!(result, Err(HostError::MemoryAboveProfile { limit, .. }) if limit == 128 << 20),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn____dense_profile____runs_instances() {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 4,
        memory_profile: MemoryProfile::Dense,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&fixture("paddock_fixture_chaos")).unwrap();
    let instance = rt.spawn(&app, spec("")).unwrap();

    let reply = instance.call(text("echo:dense"), LONG).await.unwrap();

    assert_eq!(body(&reply), "dense");
    instance.kill();
}

#[test]
fn load____not_a_component____refused() {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 1,
        ..RuntimeConfig::default()
    })
    .unwrap();

    let result = rt.load(b"not wasm");

    assert!(matches!(result, Err(HostError::Load(_))), "{result:?}");
}

#[test]
fn load____echo_fixture____links_against_host() {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 1,
        ..RuntimeConfig::default()
    })
    .unwrap();

    let result = rt.load(&fixture("paddock_fixture_echo"));

    assert!(result.is_ok(), "{result:?}");
}
