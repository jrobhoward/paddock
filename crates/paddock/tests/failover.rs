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
use paddock_host::transport::wire;

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

/// `paddock failover --watch --json` on node 2, read a line at a time.
struct Watch {
    _child: tokio::process::Child,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}

impl Watch {
    fn start(socket: &Path) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
            .arg("--socket")
            .arg(socket)
            .args(["--json", "failover", "--watch"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        Self {
            _child: child,
            lines: tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout)),
        }
    }

    /// The next line, as JSON, within 30 s.
    async fn next(&mut self) -> serde_json::Value {
        let line = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
            .await
            .expect("a line within 30 s")
            .unwrap()
            .expect("the watch is still running");
        serde_json::from_str(&line).unwrap()
    }

    /// Lines until one has `role`; that one.
    /// The next line with `role` and `may_run`. A promoted node's first
    /// `primary` line can come before its lease lets it run singletons.
    async fn until_standing(&mut self, role: &str, may_run: bool) -> serde_json::Value {
        loop {
            let line = self.next().await;
            if line["role"] == role && line["may_run"] == may_run {
                return line;
            }
        }
    }
}

/// ROADMAP D11, step 3: `paddock failover --watch` prints the standby's
/// standing, a line when it is promoted, `unreachable` when its node goes,
/// and the node's standing again once it is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover_watch____promotion_node_loss_and_return____a_line_each() {
    let dir = tempfile::Builder::new()
        .prefix("pfw")
        .tempdir_in("/tmp")
        .unwrap();
    let peers = keys(dir.path());
    let signer = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let pk = signer.pk.to_base64();
    let (a1, a2, a3) = (free_udp(), free_udp(), free_udp());
    let mut w = Process::start(dir.path(), 3, &pk, &witness(dir.path(), &peers, &a3)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut n1 = Process::start(dir.path(), 1, &pk, &member(1, dir.path(), &peers, &a1, &a3)).await;
    let mut n2 = Process::start(dir.path(), 2, &pk, &member(2, dir.path(), &peers, &a2, &a3)).await;
    until("node 2 standby", || {
        let c = n2.client.clone();
        async move { c.failover().await.is_ok_and(|f| f.role == "standby") }
    })
    .await;
    let mut watch = Watch::start(&dir.path().join("n2.sock"));

    let first = watch.next().await;
    n1.child.kill().await.unwrap();
    let promoted = watch.until_standing("primary", true).await;
    n2.child.kill().await.unwrap();
    let lost = watch.next().await;
    n2 = Process::start(dir.path(), 2, &pk, &member(2, dir.path(), &peers, &a2, &a3)).await;
    let back = watch.next().await;

    assert_eq!(first["role"], "standby");
    assert_eq!(first["may_run"], false);
    assert!(promoted["epoch"].as_u64().unwrap() >= 2, "{promoted}");
    assert_eq!(
        lost,
        serde_json::json!({ "role": "unreachable", "may_run": false })
    );
    assert_ne!(back["role"], "unreachable", "{back}");
    for p in [&mut n2, &mut w] {
        p.child.kill().await.unwrap();
    }
}

/// ROADMAP D10b: `paddock failover switchover` on the primary stops its
/// singleton gracefully and the standby runs it within about a lease,
/// without a node timeout; the old primary rejoins as standby. On a
/// standby it is refused, and nothing stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switchover____on_the_primary____standby_runs_the_singleton_old_primary_rejoins() {
    let dir = tempfile::Builder::new()
        .prefix("pfs")
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

    let on_standby = n2.client.switchover().await;
    let still_runs = runs(&n1.client).await;
    // Node 1 hands over once it has heard node 2's beats as a standby,
    // a heartbeat or so after node 2 says it is one.
    let mut handed = n1.client.switchover().await;
    for _ in 0..20 {
        if handed.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        handed = n1.client.switchover().await;
    }
    let asked = std::time::Instant::now();
    until("node 2 runs the singleton", || runs(&n2.client)).await;
    let took = asked.elapsed();
    until("node 1 back as standby", || role(&n1.client, "standby")).await;
    let n1_now = n1.client.failover().await.unwrap();
    let log = std::fs::read_to_string(dir.path().join("node-1.log")).unwrap();

    assert_eq!(common::kind(&on_standby.unwrap_err()), "conflict");
    assert!(still_runs, "a refused switchover stops nothing");
    assert!(handed.is_ok(), "{handed:?}");
    // Node 1 releases its lease at the witness (R14), so node 2 is granted
    // within a few 100 ms beats, under the 1.2 s lease and margin the
    // witness would otherwise wait (about 200 ms on 2026-10-20); a
    // takeover after a crash also waits the 1 s node timeout.
    assert!(took < Duration::from_millis(800), "{took:?}");
    assert_eq!((n1_now.epoch, n1_now.may_run), (2, false));
    assert!(
        log.contains("singleton stopped: planned switchover"),
        "{log}"
    );
    assert!(!runs(&n1.client).await);
    for p in [&mut n1, &mut n2, &mut w] {
        p.child.kill().await.unwrap();
    }
}

// ------------------------------------------------------------ the gate alone

/// One node of a pair whose partner never runs: with `arbiter`
/// "paths" it promotes itself once the partner is silent; with "operator"
/// it stays down and only holds its singletons.
async fn alone(arbiter: &str) -> (tempfile::TempDir, TestNode) {
    alone_for(arbiter, 1000).await
}

/// [`alone`], promoting itself (paths) `node_timeout_ms` after it starts.
async fn alone_for(arbiter: &str, node_timeout_ms: u64) -> (tempfile::TempDir, TestNode) {
    let dir = tempfile::tempdir().unwrap();
    let peers = keys(dir.path());
    let extra = format!(
        "{}[failover]\npartner = 2\narbiter = \"{arbiter}\"\nlease_ms = 1000\nmargin_ms = 200\n",
        cluster(1, dir.path(), &peers, &free_udp(), &[]).replace(
            "node_timeout_ms = 1000",
            &format!("node_timeout_ms = {node_timeout_ms}")
        )
    );
    (dir, TestNode::start(&extra).await)
}

fn singleton_bundle(node: &TestNode, version: &str, extra: serde_json::Value) -> Vec<u8> {
    let mut manifest = fast(extra);
    manifest["placement"] = serde_json::json!({ "mode": "singleton" });
    bundle(
        &node.key,
        "leader",
        version,
        &fixture("paddock_fixture_chaos"),
        manifest,
        &[],
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events____lone_node_promotes_itself____leadership_change_streamed() {
    // Three seconds before it promotes itself: time to start following.
    let (_dir, mut n) = alone_for("paths", 3000).await;
    let mut events = n.client.events().await.unwrap();

    let promoted = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let e = events.next().await.unwrap().unwrap();
            if e.kind == "failover" && e.fields["may_run"] == true {
                return e;
            }
        }
    })
    .await
    .expect("a failover event with may_run");
    let now = standing(&n).await;

    assert_eq!(promoted.event, "leadership");
    assert_eq!(promoted.fields["role"], "primary");
    assert_eq!(promoted.fields["epoch"], now.epoch);
    assert!(promoted.fields["generation"].as_u64().unwrap() >= 1);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate____singleton_deployed_to_a_primary____runs_at_once() {
    let (_dir, mut n) = alone("paths").await;
    until("node 1 primary alone", || async {
        standing(&n).await.may_run
    })
    .await;

    n.client
        .deploy(singleton_bundle(&n, "1.0.0", serde_json::json!({})))
        .await
        .unwrap();
    let status = singleton(&n).await;

    assert_eq!(status, "running");
    assert_eq!(n.client.ps().await.unwrap().len(), 1);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate____held_singleton____upgraded_rolled_back_and_removed_without_running() {
    let (_dir, mut n) = alone("operator").await;
    n.client
        .deploy(singleton_bundle(&n, "1.0.0", serde_json::json!({})))
        .await
        .unwrap();

    let upgraded = n
        .client
        .upgrade(
            "leader",
            singleton_bundle(&n, "2.0.0", serde_json::json!({})),
            &paddock::api::RolloutParams::default(),
        )
        .await
        .unwrap();
    let after_upgrade = n.client.status("leader").await.unwrap().summary;
    let rolled = n
        .client
        .rollback("leader", &paddock::api::RolloutParams::default())
        .await
        .unwrap();
    let after_rollback = n.client.status("leader").await.unwrap().summary;
    let instances = n.client.ps().await.unwrap().len();
    n.client.remove("leader").await.unwrap();

    assert_eq!(
        (
            upgraded.from.as_str(),
            upgraded.to.as_str(),
            upgraded.mode.as_str()
        ),
        ("1.0.0", "2.0.0", "held")
    );
    assert_eq!(
        (
            after_upgrade.version.as_str(),
            after_upgrade.status.as_str()
        ),
        ("2.0.0", "standby")
    );
    assert_eq!(
        (rolled.to.as_str(), rolled.mode.as_str()),
        ("1.0.0", "held")
    );
    assert_eq!(after_rollback.version, "1.0.0");
    assert_eq!(instances, 0);
    assert!(standing(&n).await.singletons.is_empty());
    assert!(n.client.list().await.unwrap().is_empty());
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate____singleton_start_refused_on_a_primary____deploy_refused_and_not_held() {
    let (_dir, mut n) = alone("paths").await;
    until("node 1 primary alone", || async {
        standing(&n).await.may_run
    })
    .await;

    let deployed = n
        .client
        .deploy(singleton_bundle(
            &n,
            "1.0.0",
            serde_json::json!({ "dependencies": [{ "app": "absent", "version": "^1" }] }),
        ))
        .await;

    let err = deployed.unwrap_err().to_string();
    assert!(err.contains("absent"), "{err}");
    assert!(standing(&n).await.singletons.is_empty());
    assert!(n.client.list().await.unwrap().is_empty());
    n.shutdown().await;
}

/// A new epoch that cannot be written to disk keeps the singletons
/// stopped, though the node is primary: running at an epoch a restart
/// could lose would break fencing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate____epoch_cannot_be_kept____singleton_stays_stopped() {
    let (_dir, mut n) = alone("paths").await;
    until("node 1 primary alone", || async {
        standing(&n).await.may_run
    })
    .await;
    n.client
        .deploy(singleton_bundle(&n, "1.0.0", serde_json::json!({})))
        .await
        .unwrap();
    // The epoch is written through tmp/failover-epoch; a directory there
    // makes the next write fail.
    std::fs::create_dir(n.data().join("tmp/failover-epoch")).unwrap();

    n.restart().await;
    until("node 1 primary again, at a new epoch", || async {
        let f = standing(&n).await;
        f.may_run && f.epoch == 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let status = singleton(&n).await;

    assert_eq!(status, "standby");
    assert_eq!(n.client.ps().await.unwrap().len(), 0);
    n.shutdown().await;
}

/// A node that does not run singletons checks one as a start would, so it
/// refuses what the primary would refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate____held_singleton_with_a_missing_dependency____deploy_and_upgrade_refused() {
    let (_dir, mut n) = alone("operator").await;
    let needs_base = serde_json::json!({ "dependencies": [{ "app": "base", "version": "^1" }] });

    let deployed = n
        .client
        .deploy(singleton_bundle(&n, "1.0.0", needs_base.clone()))
        .await;
    n.client
        .deploy(singleton_bundle(&n, "1.0.0", serde_json::json!({})))
        .await
        .unwrap();
    let upgraded = n
        .client
        .upgrade(
            "leader",
            singleton_bundle(&n, "2.0.0", needs_base),
            &paddock::api::RolloutParams::default(),
        )
        .await;

    let deploy_err = deployed.unwrap_err().to_string();
    let upgrade_err = upgraded.unwrap_err().to_string();
    assert!(deploy_err.contains("base"), "{deploy_err}");
    assert!(upgrade_err.contains("base"), "{upgrade_err}");
    assert_eq!(
        n.client.status("leader").await.unwrap().summary.version,
        "1.0.0"
    );
    n.shutdown().await;
}

/// A singleton that cannot start at promotion (its dependency was stopped
/// while it was held) starts once the dependency is back, without another
/// promotion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate____start_fails_at_promotion____tried_again_until_it_starts() {
    let (_dir, mut n) = alone_for("paths", 4000).await;
    n.client
        .deploy(bundle(
            &n.key,
            "base",
            "1.0.0",
            &fixture("paddock_fixture_chaos"),
            fast(serde_json::json!({})),
            &[],
        ))
        .await
        .unwrap();
    n.client
        .deploy(singleton_bundle(
            &n,
            "1.0.0",
            serde_json::json!({ "dependencies": [{ "app": "base", "version": "^1" }] }),
        ))
        .await
        .unwrap();
    n.client.stop("base").await.unwrap();
    assert!(
        !standing(&n).await.may_run,
        "promoted too early for the test"
    );

    until("node 1 primary alone", || async {
        standing(&n).await.may_run
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let while_base_stopped = singleton(&n).await;
    n.client.start("base").await.unwrap();
    until("the singleton started", || async {
        singleton(&n).await == "running"
    })
    .await;

    assert_eq!(while_base_stopped, "standby");
    n.shutdown().await;
}

/// The pair's serial line (ROADMAP F5): two ptys joined by a relay the test
/// can cut, or with `PADDOCK_SERIAL_PAIR=/dev/ttyUSB0,/dev/ttyUSB1` a pty
/// relayed to the first device while node 2 opens the second, so beats
/// cross a real cable. The devices each node of the pair opens, the cut
/// switch, and the relay's task.
async fn serial_line() -> (
    [String; 2],
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
) {
    use paddock::serial::{Tty, pty, relay};
    let (cut, cut_rx) = tokio::sync::watch::channel(false);
    let (end1, device1) = pty().unwrap();
    let (end2, device2) = match std::env::var("PADDOCK_SERIAL_PAIR") {
        Ok(pair) => {
            let (a, b) = pair.split_once(',').expect("two devices, comma-separated");
            let a = Tty::open(Path::new(a), 115_200).unwrap();
            (a, std::path::PathBuf::from(b))
        }
        Err(_) => pty().unwrap(),
    };
    let task = tokio::spawn(relay(end1, end2, cut_rx));
    let devices = [device1, device2].map(|d| d.display().to_string());
    (devices, cut, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover_serial____line_cut_and_restored_then_primary_killed____paths_follow_and_standby_takes_over()
 {
    let dir = tempfile::Builder::new()
        .prefix("pfs")
        .tempdir_in("/tmp")
        .unwrap();
    let peers = keys(dir.path());
    let signer = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let pk = signer.pk.to_base64();
    let (a1, a2, a3) = (free_udp(), free_udp(), free_udp());
    let (devices, cut, relay) = serial_line().await;
    let with_serial = |n: u32, address: &str| {
        format!(
            "{}serial = {{ device = {:?} }}\n",
            member(n, dir.path(), &peers, address, &a3),
            devices[usize::try_from(n - 1).unwrap()]
        )
    };
    let mut w = Process::start(dir.path(), 3, &pk, &witness(dir.path(), &peers, &a3)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut n1 = Process::start(dir.path(), 1, &pk, &with_serial(1, &a1)).await;
    let mut n2 = Process::start(dir.path(), 2, &pk, &with_serial(2, &a2)).await;
    let serial = |c: &paddock::api::Client, heard: bool| {
        let c = c.clone();
        async move {
            c.failover()
                .await
                .is_ok_and(|f| f.paths.get("serial") == Some(&heard))
        }
    };
    let role = |c: &paddock::api::Client, r: &'static str| {
        let c = c.clone();
        async move { c.failover().await.is_ok_and(|f| f.role == r) }
    };

    until("node 2 standby", || role(&n2.client, "standby")).await;
    until("node 1 hears serial", || serial(&n1.client, true)).await;
    until("node 2 hears serial", || serial(&n2.client, true)).await;
    cut.send_replace(true);
    until("node 2: serial silent", || serial(&n2.client, false)).await;
    let during_cut = n2.client.failover().await.unwrap();
    cut.send_replace(false);
    until("node 2 hears serial again", || serial(&n2.client, true)).await;
    n1.child.kill().await.unwrap();
    until("node 2 primary", || role(&n2.client, "primary")).await;
    let promoted = n2.client.failover().await.unwrap();

    assert_eq!(during_cut.role, "standby", "{during_cut:?}");
    assert_eq!(during_cut.paths.get("link"), Some(&true), "{during_cut:?}");
    assert_eq!(promoted.paths.get("serial"), Some(&false), "{promoted:?}");
    relay.abort();
    for p in [&mut n2, &mut w] {
        p.child.kill().await.unwrap();
    }
}

/// The SDK fixture as a singleton with its own `replicated` bucket
/// `state` (ROADMAP K3).
fn replicated_singleton(signer: &minisign::KeyPair) -> Vec<u8> {
    let manifest = fast(serde_json::json!({
        "placement": { "mode": "singleton" },
        "capabilities": {
            "wasi:config": {},
            "wasi:keyvalue": { "buckets": { "state": { "durability": "replicated" } } },
            "paddock:groups": {},
            "paddock:metrics": { "prefix": "vault" },
            "paddock:secrets": { "secrets": [] },
            // The SDK fixture imports `paddock:sql` and `wasi:http`;
            // declared, nothing used.
            "paddock:sql": {},
            "wasi:http/outgoing-handler": {}
        }
    }));
    bundle(
        signer,
        "vault",
        "1.0.0",
        &fixture("paddock_fixture_sdk"),
        manifest,
        &[],
    )
}

async fn call_text(c: &paddock::api::Client, app: &str, body: &str) -> String {
    match c
        .call(app, "text/plain", body.as_bytes().to_vec(), 5_000)
        .await
    {
        Ok((_, reply)) => String::from_utf8(reply).unwrap(),
        Err(e) => format!("error: {e}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover____singleton_with_replicated_bucket_primary_killed____new_primary_has_every_write()
 {
    let dir = tempfile::Builder::new()
        .prefix("pfr")
        .tempdir_in("/tmp")
        .unwrap();
    let peers = keys(dir.path());
    let signer = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let pk = signer.pk.to_base64();
    let (a1, a2, a3) = (free_udp(), free_udp(), free_udp());
    let policy = dir.path().join("policy.toml");
    std::fs::write(&policy, "[apps.vault]\ngrant = [\"wasi:keyvalue\"]\n").unwrap();
    let replicating = |n: u32, a: &str| {
        format!(
            "policy = {policy:?}\n{}replication = true\n",
            member(n, dir.path(), &peers, a, &a3)
        )
    };
    let mut w = Process::start(dir.path(), 3, &pk, &witness(dir.path(), &peers, &a3)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut n1 = Process::start(dir.path(), 1, &pk, &replicating(1, &a1)).await;
    let mut n2 = Process::start(dir.path(), 2, &pk, &replicating(2, &a2)).await;
    n1.client
        .deploy(replicated_singleton(&signer))
        .await
        .unwrap();
    n2.client
        .deploy(replicated_singleton(&signer))
        .await
        .unwrap();
    let runs = |c: &paddock::api::Client| {
        let c = c.clone();
        async move {
            c.status("vault")
                .await
                .is_ok_and(|d| d.summary.status == "running")
        }
    };
    until("node 1 runs the singleton", || runs(&n1.client)).await;
    until("node 2 standby", || {
        let c = n2.client.clone();
        async move { c.failover().await.is_ok_and(|f| f.role == "standby") }
    })
    .await;

    let mut acked = Vec::new();
    for i in 0..20 {
        acked.push(call_text(&n1.client, "vault", &format!("set:state:k{i}:v{i}")).await);
    }
    n1.child.kill().await.unwrap();
    until("node 2 runs the singleton", || runs(&n2.client)).await;
    let last = call_text(&n2.client, "vault", "get:state:k19").await;
    let keys = call_text(&n2.client, "vault", "keys:state").await;
    let after = call_text(&n2.client, "vault", "set:state:k20:v20").await;
    let log = std::fs::read_to_string(dir.path().join("node-2.log")).unwrap();
    let waited = log.find("singleton waiting");
    let started = log.rfind("singleton started");

    assert!(acked.iter().all(|r| r == "ok"), "{acked:?}");
    // Promoted before its copy was current (the witness had to record
    // node 1 out of date first), it held the singleton until then.
    assert!(waited.is_some() && waited < started, "{log}");
    assert_eq!(last, "v19");
    assert_eq!(keys.split(',').count(), 20, "{keys}");
    assert_eq!(after, "ok");
    for p in [&mut n2, &mut w] {
        p.child.kill().await.unwrap();
    }
}

/// The counter fixture as a singleton in `snapshot` upgrade mode: it
/// moves with its count in a planned switchover (ROADMAP M1).
fn moving_counter(signer: &minisign::KeyPair) -> Vec<u8> {
    let mut manifest = common::counter_manifest(None);
    manifest["placement"] = serde_json::json!({ "mode": "singleton" });
    bundle(signer, "tally", "1.0.0", &common::counter(1), manifest, &[])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switchover____singleton_in_snapshot_mode____moves_with_its_state_once() {
    let dir = tempfile::Builder::new()
        .prefix("pfm")
        .tempdir_in("/tmp")
        .unwrap();
    let peers = keys(dir.path());
    let signer = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let pk = signer.pk.to_base64();
    let (a1, a2, a3) = (free_udp(), free_udp(), free_udp());
    let replicating = |n: u32, a: &str| {
        format!(
            "{}replication = true\n",
            member(n, dir.path(), &peers, a, &a3)
        )
    };
    let mut w = Process::start(dir.path(), 3, &pk, &witness(dir.path(), &peers, &a3)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut n1 = Process::start(dir.path(), 1, &pk, &replicating(1, &a1)).await;
    let mut n2 = Process::start(dir.path(), 2, &pk, &replicating(2, &a2)).await;
    for n in [&n1, &n2] {
        n.client.deploy(moving_counter(&signer)).await.unwrap();
    }
    let runs = |c: &paddock::api::Client| {
        let c = c.clone();
        async move {
            c.status("tally")
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
    for _ in 0..3 {
        call_text(&n1.client, "tally", "inc").await;
    }

    let mut handed = n1.client.switchover().await;
    for _ in 0..20 {
        if handed.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        handed = n1.client.switchover().await;
    }
    until("node 2 runs the singleton", || runs(&n2.client)).await;
    let moved = call_text(&n2.client, "tally", "get").await;
    call_text(&n2.client, "tally", "inc").await;
    until("node 1 back as standby", || role(&n1.client, "standby")).await;
    // A failover is no planned switchover: node 1 starts it from init,
    // and node 2's start took the record, so no old state comes back.
    n2.child.kill().await.unwrap();
    until("node 1 runs the singleton", || runs(&n1.client)).await;
    let after_failover = call_text(&n1.client, "tally", "get").await;
    let log1 = std::fs::read_to_string(dir.path().join("node-1.log")).unwrap();
    let log2 = std::fs::read_to_string(dir.path().join("node-2.log")).unwrap();

    assert!(handed.is_ok(), "{handed:?}");
    assert_eq!(moved, "1.0.0 count=3");
    assert_eq!(after_failover, "1.0.0 count=0");
    assert!(log1.contains("planned switchover, state moved"), "{log1}");
    assert!(log2.contains("singleton moved in"), "{log2}");
    for p in [&mut n1, &mut w] {
        p.child.kill().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate____replicated_bucket_on_a_node_without_replication____deploy_refused() {
    let (_dir, mut n) = alone("paths").await;

    let refused = n.client.deploy(replicated_singleton(&n.key)).await;

    let err = refused.unwrap_err().to_string();
    assert!(err.contains("does not replicate"), "{err}");
    n.shutdown().await;
}

/// The last step of a release across the pair (ROADMAP U1): `release`
/// shows the versions each node speaks and keeps, `release finalize`
/// raises the pair's version on both nodes, once, and is refused on the
/// witness, which keeps nothing replicated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_finalize____replicating_pair____both_nodes_keep_this_version() {
    let dir = tempfile::Builder::new()
        .prefix("prf")
        .tempdir_in("/tmp")
        .unwrap();
    let peers = keys(dir.path());
    let signer = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let pk = signer.pk.to_base64();
    let (a1, a2, a3) = (free_udp(), free_udp(), free_udp());
    let replicating = |n: u32, a: &str| {
        format!(
            "{}replication = true\n",
            member(n, dir.path(), &peers, a, &a3)
        )
    };
    let mut w = Process::start(dir.path(), 3, &pk, &witness(dir.path(), &peers, &a3)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut n1 = Process::start(dir.path(), 1, &pk, &replicating(1, &a1)).await;
    let mut n2 = Process::start(dir.path(), 2, &pk, &replicating(2, &a2)).await;
    let connected = |c: &paddock::api::Client| {
        let c = c.clone();
        async move { c.release().await.is_ok_and(|r| r.peers.len() == 2) }
    };
    until("node 1 connected to both", || connected(&n1.client)).await;
    until("node 2 connected to both", || connected(&n2.client)).await;
    let before = n1.client.release().await.unwrap();

    let mut finalized = n1.client.finalize_release().await;
    for _ in 0..40 {
        if finalized.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        finalized = n1.client.finalize_release().await;
    }
    let again = n2.client.finalize_release().await;
    let on_witness = w.client.finalize_release().await;
    let keeps = |c: &paddock::api::Client| {
        let c = c.clone();
        async move { c.release().await.is_ok_and(|r| r.formats == wire::VERSION) }
    };
    until("node 2 keeps this version", || keeps(&n2.client)).await;
    let after = n1.client.release().await.unwrap();

    assert_eq!(before.formats, paddock_host::FIRST_FORMATS);
    assert_eq!(
        (before.min_version, before.max_version),
        (wire::MIN_VERSION, wire::VERSION)
    );
    assert_eq!(
        before.peers,
        std::collections::BTreeMap::from([(2, wire::VERSION), (3, wire::VERSION)])
    );
    let finalized = finalized.unwrap().message;
    assert!(finalized.contains("now keeps"), "{finalized}");
    let again = again.unwrap().message;
    assert!(again.contains("already"), "{again}");
    assert_eq!(common::kind(&on_witness.unwrap_err()), "not_found");
    assert_eq!(after.formats, wire::VERSION);
    for p in [&mut n1, &mut n2, &mut w] {
        p.child.kill().await.unwrap();
    }
}

/// The counter fixture as a singleton in `snapshot` upgrade mode at
/// `version`, built from fixture `build`, accepting snapshots from `from`.
fn counter_singleton(
    signer: &minisign::KeyPair,
    version: &str,
    build: u8,
    from: Option<&str>,
) -> Vec<u8> {
    let mut manifest = common::counter_manifest(from);
    manifest["placement"] = serde_json::json!({ "mode": "singleton" });
    bundle(
        signer,
        "tally",
        version,
        &common::counter(build),
        manifest,
        &[],
    )
}

/// `paddock release <action> ... --pair <pair> --json`, and its report.
async fn release(args: &[&str], pair: &Path) -> (bool, serde_json::Value) {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
        .arg("--json")
        .arg("release")
        .args(args)
        .arg("--pair")
        .arg(pair)
        .output()
        .await
        .unwrap();
    let report = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.success(), report)
}

/// A release across the pair (ROADMAP U1): `release upgrade` moves the
/// standby, then the primary in place, keeping the singleton's state;
/// `release rollback` returns both; a version whose `load-snapshot` fails
/// is refused on the primary, and the standby already upgraded is rolled
/// back, so both nodes end where they started.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_upgrade____singleton_on_a_pair____standby_then_primary_and_aborted_when_refused() {
    let dir = tempfile::Builder::new()
        .prefix("pru")
        .tempdir_in("/tmp")
        .unwrap();
    let peers = keys(dir.path());
    let signer = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let pk = signer.pk.to_base64();
    let (a1, a2, a3) = (free_udp(), free_udp(), free_udp());
    let replicating = |n: u32, a: &str| {
        format!(
            "{}replication = true\n",
            member(n, dir.path(), &peers, a, &a3)
        )
    };
    let mut w = Process::start(dir.path(), 3, &pk, &witness(dir.path(), &peers, &a3)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut n1 = Process::start(dir.path(), 1, &pk, &replicating(1, &a1)).await;
    let mut n2 = Process::start(dir.path(), 2, &pk, &replicating(2, &a2)).await;
    for n in [&n1, &n2] {
        n.client
            .deploy(counter_singleton(&signer, "1.0.0", 1, None))
            .await
            .unwrap();
    }
    let runs = |c: &paddock::api::Client| {
        let c = c.clone();
        async move {
            c.status("tally")
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
    for _ in 0..3 {
        call_text(&n1.client, "tally", "inc").await;
    }
    let pair = dir.path().join("pair.toml");
    std::fs::write(
        &pair,
        format!(
            "[[node]]\nid = 1\nsocket = {:?}\n\n[[node]]\nid = 2\nsocket = {:?}\n",
            dir.path().join("n1.sock"),
            dir.path().join("n2.sock")
        ),
    )
    .unwrap();
    let v2 = dir.path().join("v2.rbp");
    std::fs::write(&v2, counter_singleton(&signer, "2.0.0", 2, Some("^1"))).unwrap();
    let v3 = dir.path().join("v3.rbp");
    std::fs::write(&v3, counter_singleton(&signer, "3.0.0", 3, Some("^1"))).unwrap();
    let versions = || async {
        let mut out = Vec::new();
        for n in [&n1, &n2] {
            let d = n.client.status("tally").await.unwrap();
            out.push((d.summary.version, d.summary.status));
        }
        out
    };

    let (up_ok, up) = release(&["upgrade", "tally", v2.to_str().unwrap()], &pair).await;
    let upgraded = versions().await;
    let kept = call_text(&n1.client, "tally", "get").await;
    let (again_ok, again) = release(&["upgrade", "tally", v2.to_str().unwrap()], &pair).await;
    let (back_ok, back) = release(&["rollback", "tally"], &pair).await;
    let rolled_back = versions().await;
    let (bad_ok, bad) = release(&["upgrade", "tally", v3.to_str().unwrap()], &pair).await;
    let after_bad = versions().await;

    assert!(up_ok, "{up}");
    let steps: Vec<&str> = up["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(steps.len(), 2, "{steps:?}");
    assert!(
        steps[0].starts_with("node 2 (standby): upgraded tally 1.0.0 -> 2.0.0"),
        "{steps:?}"
    );
    assert!(
        steps[1].starts_with("node 1 (primary): upgraded tally 1.0.0 -> 2.0.0 (snapshot)"),
        "{steps:?}"
    );
    assert_eq!(
        upgraded,
        [
            ("2.0.0".to_string(), "running".to_string()),
            ("2.0.0".to_string(), "standby".to_string())
        ]
    );
    assert_eq!(kept, "2.0.0 count=3 history=");
    assert!(
        again_ok && again["steps"].as_array().unwrap().is_empty(),
        "{again}"
    );
    assert!(back_ok, "{back}");
    assert_eq!(back["to"], "1.0.0");
    assert_eq!(
        rolled_back.iter().map(|v| v.0.as_str()).collect::<Vec<_>>(),
        ["1.0.0", "1.0.0"]
    );
    assert!(!bad_ok, "{bad}");
    assert_eq!(bad["outcome"], "aborted", "{bad}");
    let reason = bad["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("node 1 (primary): upgrade refused"),
        "{reason}"
    );
    let bad_steps = bad["steps"].as_array().unwrap();
    assert_eq!(bad_steps.len(), 2, "{bad}");
    assert!(
        bad_steps[1]
            .as_str()
            .unwrap()
            .starts_with("node 2: rolled back tally 3.0.0 -> 1.0.0"),
        "{bad}"
    );
    assert_eq!(
        after_bad.iter().map(|v| v.0.as_str()).collect::<Vec<_>>(),
        ["1.0.0", "1.0.0"]
    );
    for p in [&mut n1, &mut n2, &mut w] {
        p.child.kill().await.unwrap();
    }
}
