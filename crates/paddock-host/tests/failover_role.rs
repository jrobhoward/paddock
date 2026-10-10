//! The `failover-role` host event (ROADMAP D10): told to instances that
//! asked for it (a `hot-standby` app), after `init` and on every change.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use common::{ask, ask_until, fixture, spec};
use paddock_host::{Runtime, RuntimeConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_failover_role____announcing_and_other_instances____only_the_announcing_one_told() {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 4,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&fixture("paddock_fixture_chaos")).unwrap();
    rt.set_failover_role(Some("standby"));
    let mut told = spec("");
    told.announce_role = true;
    let standby = rt.spawn(&app, told).unwrap();
    let other = rt.spawn(&app, spec("")).unwrap();

    let first = ask_until(&standby, "infos", |i| i.contains("failover-role")).await;
    rt.set_failover_role(Some("primary"));
    let then = ask_until(&standby, "infos", |i| i.contains("primary")).await;
    let untold = ask(&other, "infos").await;

    assert_eq!(first, "event:failover-role:standby");
    assert_eq!(
        then,
        "event:failover-role:standby;event:failover-role:primary"
    );
    assert_eq!(untold, "");
    standby.kill();
    other.kill();
}
