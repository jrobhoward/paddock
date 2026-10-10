//! Instances bound to the node's failover lease (a singleton's,
//! `InstanceSpec::bound_to_lease`) stop running guest code when the lease
//! ends, by themselves, before anything kills them (assumption A8 in
//! `docs/tla/VERIFICATION.md`).

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{fixture, spec, text};
use paddock_host::failover::{Arbiter, FailoverConfig, WitnessConfig};
use paddock_host::transport::memory::Mesh;
use paddock_host::{ExitReason, Runtime, RuntimeConfig};

const LEASE: Duration = Duration::from_millis(600);
const MARGIN: Duration = Duration::from_millis(100);

fn node(id: u32) -> Runtime {
    Runtime::new(RuntimeConfig {
        node: id,
        max_instances: 8,
        heartbeat_interval: Duration::from_millis(50),
        node_timeout: Duration::from_millis(250),
        ..RuntimeConfig::default()
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_instance____lease_ends_mid_callback____ended_lease_ended_without_a_kill() {
    let nodes: Vec<Runtime> = (1..=3).map(node).collect();
    let mesh = Mesh::new(7);
    for rt in &nodes {
        mesh.join(rt).unwrap();
    }
    let mut witness = WitnessConfig::new([1, 2], 1);
    (witness.lease, witness.margin) = (LEASE, MARGIN);
    nodes[2].serve_witness(witness).unwrap();
    let mut cfg = FailoverConfig::new(2, Arbiter::Witness(3));
    (cfg.lease, cfg.margin) = (LEASE, MARGIN);
    let mut primary = nodes[0].start_failover(cfg).unwrap().subscribe();
    let mut cfg = FailoverConfig::new(1, Arbiter::Witness(3));
    (cfg.lease, cfg.margin) = (LEASE, MARGIN);
    let _standby = nodes[1].start_failover(cfg).unwrap();
    tokio::time::timeout(Duration::from_secs(10), primary.wait_for(|l| l.may_run))
        .await
        .unwrap()
        .unwrap();
    let app = nodes[0].load(&fixture("paddock_fixture_chaos")).unwrap();
    let mut bound = spec("");
    bound.bound_to_lease = true;
    bound.callback_deadline = Duration::from_secs(60);
    let singleton = nodes[0].spawn(&app, bound).unwrap();
    singleton.cast(text("spin")).unwrap();

    let cut = Instant::now();
    mesh.partition(1, 2);
    mesh.partition(1, 3);
    let reason = tokio::time::timeout(Duration::from_secs(10), singleton.wait())
        .await
        .unwrap();

    assert_eq!(reason, ExitReason::Crashed("lease_ended".into()));
    assert!(
        cut.elapsed() < LEASE + Duration::from_secs(1),
        "{:?}",
        cut.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_instance____no_failover_on_the_node____runs_as_any_other() {
    let rt = node(1);
    let app = rt.load(&fixture("paddock_fixture_chaos")).unwrap();
    let mut bound = spec("");
    bound.bound_to_lease = true;
    let instance = rt.spawn(&app, bound).unwrap();

    let reply = instance.call(text("echo:hi"), Duration::from_secs(5)).await;

    assert_eq!(common::body(&reply.unwrap()), "hi");
    instance.kill();
}
