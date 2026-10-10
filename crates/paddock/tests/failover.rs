//! An HA pair with a witness over the QUIC transport (ROADMAP D10): a
//! singleton runs on the primary only, moves to the standby when the
//! primary stops, and stays put when the old primary comes back.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::{TestNode, bundle, fast, fixture};
use paddock::api::FailoverInfo;
use paddock_host::transport::quic::generate_key;

fn free_udp() -> String {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string()
}

/// Keys for nodes 1 to 3 in `dir`, and a peers file listing them.
fn keys(dir: &Path) -> std::path::PathBuf {
    let mut peers = String::from("[peers]\n");
    for n in [1, 2, 3] {
        let (private, public) = generate_key().unwrap();
        std::fs::write(dir.join(format!("node-{n}.key")), private).unwrap();
        peers.push_str(&format!("{n} = [\"{public}\"]\n"));
    }
    let path = dir.join("peers.toml");
    std::fs::write(&path, peers).unwrap();
    path
}

fn cluster(node: u32, dir: &Path, peers: &Path, address: &str, seeds: &[&str]) -> String {
    format!(
        "node = {node}\n[cluster]\naddress = \"{address}\"\nseeds = {seeds:?}\n\
         key = {:?}\npeers = {peers:?}\nheartbeat_interval_ms = 100\nnode_timeout_ms = 1000\n",
        dir.join(format!("node-{node}.key"))
    )
}

fn member(node: u32, dir: &Path, peers: &Path, address: &str, seed: &str) -> String {
    format!(
        "{}[failover]\npartner = {}\narbiter = \"witness\"\nwitness = 3\n\
         lease_ms = 1000\nmargin_ms = 200\n",
        cluster(node, dir, peers, address, &[seed]),
        3 - node
    )
}

fn witness(dir: &Path, peers: &Path, address: &str) -> String {
    format!(
        "{}role = \"witness\"\n[witness]\npair = [1, 2]\nfirst = 1\nlease_ms = 1000\nmargin_ms = 200\n",
        cluster(3, dir, peers, address, &[])
    )
}

async fn until<F, Fut>(what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if done().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn standing(node: &TestNode) -> FailoverInfo {
    node.client.failover().await.unwrap()
}

/// The singleton's status on `node`.
async fn singleton(node: &TestNode) -> String {
    node.client.status("leader").await.unwrap().summary.status
}

async fn deploy_singleton(node: &TestNode) {
    node.client
        .deploy(bundle(
            &node.key,
            "leader",
            "1.0.0",
            &fixture("paddock_fixture_chaos"),
            fast(serde_json::json!({ "placement": { "mode": "singleton" } })),
            &[],
        ))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover____primary_stops____singleton_moves_to_standby_and_stays() {
    let dir = tempfile::tempdir().unwrap();
    let peers = keys(dir.path());
    let (a1, a2, a3) = (free_udp(), free_udp(), free_udp());
    let mut w = TestNode::start(&witness(dir.path(), &peers, &a3)).await;
    let mut n1 = TestNode::start(&member(1, dir.path(), &peers, &a1, &a3)).await;
    let mut n2 = TestNode::start(&member(2, dir.path(), &peers, &a2, &a3)).await;
    deploy_singleton(&n1).await;
    deploy_singleton(&n2).await;

    until("node 1 primary and running the singleton", || async {
        standing(&n1).await.may_run && singleton(&n1).await == "running"
    })
    .await;
    until("node 2 standby", || async {
        standing(&n2).await.role == "standby"
    })
    .await;
    let before = (singleton(&n2).await, n2.client.ps().await.unwrap().len());
    n1.shutdown().await;
    until("node 2 primary and running the singleton", || async {
        standing(&n2).await.may_run && singleton(&n2).await == "running"
    })
    .await;
    let promoted = standing(&n2).await;
    n1.release();
    n1.restart().await;
    until("node 1 back as standby", || async {
        standing(&n1).await.role == "standby"
    })
    .await;
    let rejoined = (singleton(&n1).await, standing(&n1).await);
    let kept: u64 = std::fs::read_to_string(n2.data().join("failover-epoch"))
        .unwrap()
        .parse()
        .unwrap();

    assert_eq!(before, ("standby".to_string(), 0));
    assert!(promoted.epoch > 1, "{promoted:?}");
    assert_eq!(kept, promoted.epoch);
    assert_eq!(rejoined.0, "standby");
    assert!(!rejoined.1.may_run);
    assert_eq!(rejoined.1.epoch, promoted.epoch);
    assert_eq!(rejoined.1.singletons, vec!["leader".to_string()]);
    assert!(singleton(&n2).await == "running");
    n2.shutdown().await;
    n1.shutdown().await;
    w.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover____node_without_failover____not_found() {
    let node = TestNode::start("").await;

    let err = node.client.failover().await.unwrap_err();

    assert_eq!(common::kind(&err), "not_found");
}
