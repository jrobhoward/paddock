//! An HA pair with a witness over the QUIC transport (ROADMAP D10): a
//! singleton runs on the primary only, moves to the standby when the
//! primary stops or its process is killed, and stays put when the old
//! primary comes back.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
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
        let key = dir.join(format!("node-{n}.key"));
        std::fs::write(&key, private).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
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

/// A `paddock node` process.
struct Process {
    child: tokio::process::Child,
    client: paddock::api::Client,
}

impl Process {
    /// Start `paddock node` on `config` (TOML, without `data_dir` and the
    /// socket, which go in `dir`) and wait for its admin socket.
    async fn start(dir: &Path, n: u32, signer: &str, config: &str) -> Self {
        let socket = dir.join(format!("n{n}.sock"));
        let _ = std::fs::remove_file(&socket);
        let path = dir.join(format!("node-{n}.toml"));
        std::fs::write(
            &path,
            format!(
                "data_dir = {:?}\nadmin_socket = {socket:?}\nmax_instances = 32\n\
                 trusted_keys = [{signer:?}]\n{config}",
                dir.join(format!("data-{n}"))
            ),
        )
        .unwrap();
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("node-{n}.log")))
            .unwrap();
        let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
            .arg("node")
            .arg("--config")
            .arg(&path)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        for _ in 0..400 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Self {
            child,
            client: paddock::api::Client::new(&socket),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover____primary_process_killed____standby_takes_over_and_old_primary_rejoins() {
    // A short directory: the admin sockets live in it.
    let dir = tempfile::Builder::new()
        .prefix("pfo")
        .tempdir_in("/tmp")
        .unwrap();
    let peers = keys(dir.path());
    let signer = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let pk = signer.pk.to_base64();
    let (a1, a2, a3) = (free_udp(), free_udp(), free_udp());
    let app = bundle(
        &signer,
        "leader",
        "1.0.0",
        &fixture("paddock_fixture_chaos"),
        fast(serde_json::json!({ "placement": { "mode": "singleton" } })),
        &[],
    );
    let mut w = Process::start(dir.path(), 3, &pk, &witness(dir.path(), &peers, &a3)).await;
    // The pair starts after the witness has given up on hearing it.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut n1 = Process::start(dir.path(), 1, &pk, &member(1, dir.path(), &peers, &a1, &a3)).await;
    let mut n2 = Process::start(dir.path(), 2, &pk, &member(2, dir.path(), &peers, &a2, &a3)).await;
    n1.client.deploy(app.clone()).await.unwrap();
    n2.client.deploy(app).await.unwrap();
    let runs = |c: &paddock::api::Client| {
        let c = c.clone();
        async move {
            c.status("leader")
                .await
                .is_ok_and(|d| d.summary.status == "running")
        }
    };
    let role = |c: &paddock::api::Client, r: &'static str| {
        let c = c.clone();
        async move { c.failover().await.is_ok_and(|f| f.role == r) }
    };

    until("node 1 runs the singleton", || runs(&n1.client)).await;
    until("node 2 standby", || role(&n2.client, "standby")).await;
    n1.child.kill().await.unwrap();
    let killed = std::time::Instant::now();
    until("node 2 runs the singleton", || runs(&n2.client)).await;
    let took = killed.elapsed();
    n1 = Process::start(dir.path(), 1, &pk, &member(1, dir.path(), &peers, &a1, &a3)).await;
    until("node 1 back as standby", || role(&n1.client, "standby")).await;
    let back = n1.client.failover().await.unwrap();

    // node timeout + lease + 2 margins = 1 s + 1 s + 0.4 s, plus beats.
    assert!(took < Duration::from_secs(8), "{took:?}");
    assert!(!back.may_run);
    assert!(back.epoch >= 2, "{back:?}");
    assert!(runs(&n2.client).await);
    for p in [&mut n1, &mut n2, &mut w] {
        p.child.kill().await.unwrap();
    }
}
