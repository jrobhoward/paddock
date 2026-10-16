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
    async fn until_role(&mut self, role: &str) -> serde_json::Value {
        loop {
            let line = self.next().await;
            if line["role"] == role {
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
    let promoted = watch.until_role("primary").await;
    n2.child.kill().await.unwrap();
    let lost = watch.next().await;
    n2 = Process::start(dir.path(), 2, &pk, &member(2, dir.path(), &peers, &a2, &a3)).await;
    let back = watch.next().await;

    assert_eq!(first["role"], "standby");
    assert_eq!(first["may_run"], false);
    assert_eq!(promoted["may_run"], true);
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
    // lease + 2 margins + beats = 1 s + 0.4 s + a few 100 ms beats; a
    // takeover after a crash also waits the 1 s node timeout.
    assert!(took < Duration::from_millis(2_500), "{took:?}");
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
