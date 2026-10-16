//! Two nodes over the QUIC transport (ROADMAP D1): joined through a seed
//! address with pinned keys, a call from one node's admin API to an
//! instance on the other, and a clean leave.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use common::{TestNode, bundle, fast, fixture};
use paddock::api::Target;
use paddock_host::transport::quic::generate_key;

fn free_udp() -> String {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string()
}

/// Keys for nodes 1 and 2 in `dir`, and a peers file listing both.
fn keys(dir: &Path) -> std::path::PathBuf {
    let mut peers = String::from("[peers]\n");
    for n in [1, 2] {
        let (private, public) = generate_key().unwrap();
        let key = dir.join(format!("node-{n}.key"));
        std::fs::write(&key, private).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        peers.push_str(&format!("{n} = [\"{public}\"]\n"));
    }
    let path = dir.join("peers.toml");
    std::fs::write(&path, peers).unwrap();
    path
}

fn config(node: u32, dir: &Path, peers: &Path, address: &str, seeds: &[&str]) -> String {
    format!(
        "node = {node}\n[cluster]\naddress = \"{address}\"\nseeds = {seeds:?}\n\
         key = {:?}\npeers = {peers:?}\nheartbeat_interval_ms = 100\nnode_timeout_ms = 1000\n",
        dir.join(format!("node-{node}.key"))
    )
}

async fn until<F, Fut>(what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..300 {
        if done().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cluster____two_nodes_by_seed____members_remote_call_leave_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let peers = keys(dir.path());
    let (a1, a2) = (free_udp(), free_udp());
    let mut n1 = TestNode::start(&config(1, dir.path(), &peers, &a1, &[])).await;
    let mut events = n1.client.events().await.unwrap();
    let mut n2 = TestNode::start(&config(2, dir.path(), &peers, &a2, &[&a1])).await;

    until("both members reachable", || async {
        let m = n1.client.members().await.unwrap();
        m.len() == 2 && m.iter().all(|m| m.reachable)
    })
    .await;
    n2.client
        .deploy(bundle(
            &n2.key,
            "chaos",
            "1.0.0",
            &fixture("paddock_fixture_chaos"),
            fast(serde_json::json!({})),
            &[],
        ))
        .await
        .unwrap();
    let pid = n2.client.ps().await.unwrap()[0].pid.clone();
    let (_, reply) = n1
        .client
        .call_to(
            &Target::Pid(pid.clone()),
            "text/plain",
            b"echo:across".to_vec(),
            5_000,
        )
        .await
        .unwrap();
    n2.shutdown().await;
    until("node 2 gone", || async {
        n1.client.members().await.unwrap().len() == 1
    })
    .await;
    let mut seen = Vec::new();
    while !seen.contains(&"left".to_string()) {
        let e = tokio::time::timeout(Duration::from_secs(10), events.next())
            .await
            .expect("member events")
            .unwrap()
            .unwrap();
        if e.kind == "member" && e.fields["node"] == 2 {
            seen.push(e.event);
        }
    }

    assert!(pid.starts_with("2."), "{pid}");
    assert_eq!(reply, b"across");
    assert_eq!(seen.first().map(String::as_str), Some("joined"), "{seen:?}");
    n1.shutdown().await;
}
