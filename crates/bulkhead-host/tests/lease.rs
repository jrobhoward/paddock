//! Instances bound to the node's failover lease (a singleton's,
//! `InstanceSpec::bound_to_lease`) stop running guest code when the lease
//! ends, by themselves, before anything kills them (assumption A8 in
//! `docs/tla/VERIFICATION.md`).

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use bulkhead_host::failover::{Arbiter, FailoverConfig, WitnessConfig};
use bulkhead_host::in_process::chaos::Chaos;
use bulkhead_host::transport::memory::Mesh;
use bulkhead_host::{ExitReason, Runtime, RuntimeConfig};
use common::{spec, text};

const LEASE: Duration = Duration::from_millis(600);
const MARGIN: Duration = Duration::from_millis(100);

fn node(id: u32) -> Runtime {
    let mut native = bulkhead_host::NativeConfig::default();
    if cfg!(target_os = "linux") {
        native.runner = Some(common::native_fixtures().0.clone());
    }
    Runtime::new(RuntimeConfig {
        node: id,
        max_instances: 8,
        heartbeat_interval: Duration::from_millis(50),
        node_timeout: Duration::from_millis(250),
        native,
        ..RuntimeConfig::default()
    })
    .unwrap()
}

/// Node 1 primary of a pair with node 2, witness node 3, holding its
/// lease; `mesh.partition(1, 2)` and `(1, 3)` end it.
async fn primary_with_a_lease() -> (Vec<Runtime>, Mesh) {
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
    // The standby's handle ends with the runtime, which the caller keeps.
    std::mem::forget(nodes[1].start_failover(cfg).unwrap());
    tokio::time::timeout(Duration::from_secs(10), primary.wait_for(|l| l.may_run))
        .await
        .unwrap()
        .unwrap();
    (nodes, mesh)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_instance____lease_ends_mid_callback____ended_lease_ended_without_a_kill() {
    let (nodes, mesh) = primary_with_a_lease().await;
    let app = nodes[0].load_in_process::<Chaos>();
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
    let app = rt.load_in_process::<Chaos>();
    let mut bound = spec("");
    bound.bound_to_lease = true;
    let instance = rt.spawn(&app, bound).unwrap();

    let reply = instance.call(text("echo:hi"), Duration::from_secs(5)).await;

    assert_eq!(common::body(&reply.unwrap()), "hi");
    instance.kill();
}

/// A native singleton bound to the lease: the native fixture through
/// `bulkhead-runner`.
#[cfg(target_os = "linux")]
fn native_singleton(rt: &Runtime) -> bulkhead_host::InstanceHandle {
    let app = rt.load_native(bulkhead_host::NativeCode {
        path: common::native_fixtures().1.clone(),
        kind: bulkhead_bundle::manifest::NativeProgram::Executable,
    });
    let mut bound = bulkhead_host::InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_secs(60),
        init_timeout: Duration::from_secs(5),
        bound_to_lease: true,
        ..bulkhead_host::InstanceSpec::new("native-singleton", "1.0.0")
    };
    bound.shutdown_timeout = Duration::from_secs(1);
    rt.spawn(&app, bound).unwrap()
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_bound_instance____lease_ends_mid_callback____crashed_at_the_lease_end() {
    let (nodes, mesh) = primary_with_a_lease().await;
    let singleton = native_singleton(&nodes[0]);
    assert_eq!(common::ask(&singleton, "echo up").await, "up");
    // The fixture runs commands it is called with (casts it only records).
    let spinning = singleton.clone();
    tokio::spawn(async move { spinning.call(text("spin"), Duration::from_secs(30)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let cut = Instant::now();
    mesh.partition(1, 2);
    mesh.partition(1, 3);
    let reason = tokio::time::timeout(Duration::from_secs(10), singleton.wait())
        .await
        .unwrap();

    // The node and the runner's watchdog act at the same end: whichever
    // the node finds first.
    assert!(
        [
            ExitReason::Crashed("lease_ended".into()),
            ExitReason::Crashed("killed_at_lease_end".into())
        ]
        .contains(&reason),
        "{reason:?}"
    );
    assert!(
        cut.elapsed() < LEASE + Duration::from_secs(1),
        "{:?}",
        cut.elapsed()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_bound_instance____lease_ends_while_idle____crashed_at_the_lease_end() {
    // A native process can run threads between callbacks, so an idle one
    // is ended too, not left to wait for its next message.
    let (nodes, mesh) = primary_with_a_lease().await;
    let singleton = native_singleton(&nodes[0]);
    assert_eq!(common::ask(&singleton, "echo up").await, "up");

    let cut = Instant::now();
    mesh.partition(1, 2);
    mesh.partition(1, 3);
    let reason = tokio::time::timeout(Duration::from_secs(10), singleton.wait())
        .await
        .unwrap();

    // The node and the runner's watchdog act at the same end: whichever
    // the node finds first.
    assert!(
        [
            ExitReason::Crashed("lease_ended".into()),
            ExitReason::Crashed("killed_at_lease_end".into())
        ]
        .contains(&reason),
        "{reason:?}"
    );
    assert!(
        cut.elapsed() < LEASE + Duration::from_secs(1),
        "{:?}",
        cut.elapsed()
    );
}
