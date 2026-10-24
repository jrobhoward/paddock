//! The board example (`examples/board`, chapter 1 of `examples/tutorial`):
//! its manifests bundled with the CLI, served over HTTP from a shared disk
//! bucket; a crash restarts the instance and the count survives it, the
//! watcher logs each change, and the count survives a node restart.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

mod common;

use paddock::node::{Node, NodeConfig};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn build() -> PathBuf {
    let target = root().join("target/fixtures/examples-board");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = StdCommand::new(cargo)
        .current_dir(root())
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .args(["-p", "example-board", "--target-dir"])
        .arg(&target)
        .status()
        .unwrap()
        .success();
    assert!(ok, "building example-board failed");
    target.join("wasm32-wasip2/release/example_board.wasm")
}

async fn paddock(socket: &Path, args: &[&str]) -> (bool, String, String) {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .await
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One request on its own connection: (status, body), or `None` if the
/// connection fails.
async fn http(port: u16, method: &str, path: &str) -> Option<(u16, String)> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.ok()?;
    let (head, body) = raw.split_once("\r\n\r\n")?;
    Some((head.split(' ').nth(1)?.parse().ok()?, body.to_string()))
}

/// The board's JSON reply: (count, pid).
async fn board(port: u16) -> (i64, String) {
    for _ in 0..200 {
        if let Some((200, body)) = http(port, "GET", "/board").await {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            return (
                v["count"].as_i64().unwrap(),
                v["pid"].as_str().unwrap().to_string(),
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the board never answered");
}

/// A node and its admin API.
struct Running {
    node: Arc<Node>,
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

impl Running {
    /// Open a node on `toml`, retrying while a previous node on the same
    /// data still holds it, and serve its admin API.
    async fn open(toml: &str, socket: &Path) -> Self {
        let mut tries = 0;
        let node = loop {
            match Node::open(NodeConfig::from_toml_str(toml).unwrap(), common::ring()).await {
                Ok(node) => break Arc::new(node),
                Err(_) if tries < 250 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("{e}"),
            }
        };
        let _ = std::fs::remove_file(socket);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn({
            let node = node.clone();
            async move {
                let _ = paddock::node::serve(node, async {
                    let _ = stopped.await;
                })
                .await;
            }
        });
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Self { node, stop, server }
    }

    /// Stop the apps and the API and release the data directory.
    async fn close(self) {
        self.node.shutdown().await;
        let _ = self.stop.send(());
        let _ = self.server.await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn board____crash_and_node_restart____count_survives_and_watcher_logs_changes() {
    let wasm = build();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("a.sock");
    let (ok, public, err) = paddock(
        &socket,
        &["keygen", "--no-password", s(&dir.path().join("k"))],
    )
    .await;
    assert!(ok, "{err}");
    let key = dir.path().join("k.key");
    let mut bundles = Vec::new();
    for manifest in ["paddock.toml", "watch.toml"] {
        let out = dir.path().join(manifest.replace(".toml", ".rbp"));
        let manifest = root().join("examples/board").join(manifest);
        let (ok, _, err) = paddock(
            &socket,
            &[
                "bundle",
                s(&manifest),
                s(&wasm),
                "--key",
                s(&key),
                "-o",
                s(&out),
            ],
        )
        .await;
        assert!(ok, "{err}");
        bundles.push(out);
    }
    let policy = dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.board]\ngrant = [\"wasi:http/incoming-handler\", \"wasi:keyvalue\", \"wasi:logging\"]\n\
         buckets = [\"board\"]\n\
         [apps.board-watch]\ngrant = [\"wasi:keyvalue\", \"wasi:logging\"]\nbuckets = [\"board\"]\n",
    )
    .unwrap();
    let port = free_port();
    let toml = format!(
        "data_dir = {:?}\nadmin_socket = {socket:?}\ntrusted_keys = [{public:?}]\npolicy = {policy:?}\n\
         [http.main]\naddress = \"127.0.0.1:{port}\"\n[buckets.board]\ndurability = \"disk\"\n",
        dir.path().join("data")
    );
    let node = Running::open(&toml, &socket).await;
    for b in &bundles {
        let (ok, _, err) = paddock(&socket, &["deploy", s(b)]).await;
        assert!(ok, "{err}");
    }

    for _ in 0..3 {
        http(port, "POST", "/board").await.unwrap();
    }
    let before = board(port).await;
    let crash = http(port, "POST", "/board/crash").await.unwrap();
    let after_crash = board(port).await;
    let (_, logs, _) = paddock(&socket, &["logs", "board-watch"]).await;
    node.close().await;
    let node = Running::open(&toml, &socket).await;
    let after_restart = board(port).await;

    assert_eq!(before.0, 3);
    assert_eq!(crash.0, 502);
    assert_eq!(after_crash.0, 3);
    // Same instance id, next incarnation: <1.1.1@1> then <1.1.2@1>.
    assert_ne!(after_crash.1, before.1);
    assert_eq!(
        after_crash
            .1
            .split('@')
            .next()
            .unwrap()
            .rsplit_once('.')
            .unwrap()
            .0,
        before
            .1
            .split('@')
            .next()
            .unwrap()
            .rsplit_once('.')
            .unwrap()
            .0
    );
    assert!(logs.contains("count is 3"), "{logs}");
    assert_eq!(after_restart.0, 3);
    node.close().await;
}

// ------------------------------------------------- chapter 3: hot standby

/// The cluster tables of node `n`'s config (its `node` id goes first): a pair (1, 2) with a witness (3),
/// short timings, keys in `dir`.
fn pair_config(n: u32, dir: &Path, addresses: &[String; 3]) -> String {
    let i = usize::try_from(n - 1).unwrap();
    let seeds = if n == 3 {
        String::new()
    } else {
        format!("seeds = [\"{}\"]\n", addresses[2])
    };
    let mut c = format!(
        "[cluster]\naddress = \"{}\"\n{seeds}key = {:?}\npeers = {:?}\n\
         heartbeat_interval_ms = 100\nnode_timeout_ms = 1000\n",
        addresses[i],
        dir.join(format!("node-{n}.key")),
        dir.join("peers.toml"),
    );
    if n == 3 {
        c += "role = \"witness\"\n[witness]\npair = [1, 2]\nfirst = 1\n";
    } else {
        c += &format!(
            "[failover]\npartner = {}\narbiter = \"witness\"\nwitness = 3\n",
            3 - n
        );
    }
    c + "lease_ms = 1000\nmargin_ms = 200\n"
}

/// A `paddock node` process, killed when dropped.
struct Process {
    child: tokio::process::Child,
    socket: PathBuf,
}

impl Process {
    async fn start(dir: &Path, n: u32, toml: &str) -> Self {
        let socket = dir.join(format!("n{n}.sock"));
        let _ = std::fs::remove_file(&socket);
        let path = dir.join(format!("node-{n}.toml"));
        std::fs::write(&path, toml).unwrap();
        let log = std::fs::File::create(dir.join(format!("node-{n}.log"))).unwrap();
        let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
            .args(["node", "--config"])
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
        Self { child, socket }
    }

    async fn role(&self) -> String {
        paddock::api::Client::new(&self.socket)
            .failover()
            .await
            .map(|f| f.role)
            .unwrap_or_default()
    }
}

/// A write to the board on `port`: (status, count, replicas).
async fn write(port: u16) -> (u16, i64, i64) {
    let (status, body) = http(port, "POST", "/board").await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    (
        status,
        v["count"].as_i64().unwrap_or(-1),
        v["replicas"].as_i64().unwrap_or(-1),
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

/// Three `paddock node` processes in `d`: a witness and the pair, each of
/// the pair with an HTTP listener on its own port and `http_extra(n)` in
/// that listener's table, the `board` bucket on disk (with `bucket_extra`
/// in its table), and the board bundled from `manifest` deployed to both.
/// Waits for node 1 primary and node 2 standby.
struct Pair {
    _witness: Process,
    n1: Process,
    n2: Process,
    ports: [u16; 2],
}

async fn start_pair(
    d: &Path,
    manifest: &Path,
    grant: &str,
    http_extra: impl Fn(u32, &[u16; 2]) -> String,
    bucket_extra: &str,
) -> Pair {
    use std::os::unix::fs::PermissionsExt;

    let wasm = build();
    let socket = d.join("cli.sock");
    let (ok, signer, err) = paddock(&socket, &["keygen", "--no-password", s(&d.join("k"))]).await;
    assert!(ok, "{err}");
    let bundle = d.join("board.rbp");
    let (ok, _, err) = paddock(
        &socket,
        &[
            "bundle",
            s(manifest),
            s(&wasm),
            "--key",
            s(&d.join("k.key")),
            "-o",
            s(&bundle),
        ],
    )
    .await;
    assert!(ok, "{err}");
    let mut peers = String::from("[peers]\n");
    for n in 1..=3 {
        let (private, public) = paddock_host::transport::quic::generate_key().unwrap();
        let key = d.join(format!("node-{n}.key"));
        std::fs::write(&key, private).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        peers.push_str(&format!("{n} = [\"{public}\"]\n"));
    }
    std::fs::write(d.join("peers.toml"), peers).unwrap();
    std::fs::write(
        d.join("policy.toml"),
        format!("[apps.board]\ngrant = [{grant}]\nbuckets = [\"board\"]\n"),
    )
    .unwrap();
    let addresses = [0, 1, 2].map(|_| {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string()
    });
    let ports = [free_port(), free_port()];
    let toml = |n: u32| {
        let mut t = format!(
            "node = {n}\ndata_dir = {:?}\nadmin_socket = {:?}\nmax_instances = 32\ntrusted_keys = [{signer:?}]\n\
             policy = {:?}\n",
            d.join(format!("data-{n}")),
            d.join(format!("n{n}.sock")),
            d.join("policy.toml"),
        );
        if n != 3 {
            let port = ports[usize::try_from(n - 1).unwrap()];
            t += &format!(
                "[http.main]\naddress = \"127.0.0.1:{port}\"\n{}[buckets.board]\ndurability = \"disk\"\n{bucket_extra}",
                http_extra(n, &ports)
            );
        }
        t + &pair_config(n, d, &addresses)
    };
    let witness = Process::start(d, 3, &toml(3)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let n1 = Process::start(d, 1, &toml(1)).await;
    let n2 = Process::start(d, 2, &toml(2)).await;
    for p in [&n1, &n2] {
        let (ok, _, err) = paddock(&p.socket, &["deploy", s(&bundle)]).await;
        assert!(ok, "{err}");
    }
    until("node 1 primary", || async { n1.role().await == "primary" }).await;
    until("node 2 standby", || async { n2.role().await == "standby" }).await;
    Pair {
        _witness: witness,
        n1,
        n2,
        ports,
    }
}

/// A short directory: the admin sockets live in it.
fn short_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("pbh")
        .tempdir_in("/tmp")
        .unwrap()
}

/// Tutorial chapter 3 (process groups across nodes): the board as a hot standby on an
/// HA pair. The primary puts each write on the standby's board, through a
/// process group across the nodes, before it answers, so the count survives
/// `kill -9` of the primary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn board____hot_standby_primary_killed____count_survives_on_the_standby() {
    let dir = short_dir();
    let manifest = root().join("examples/board/hot-standby.toml");
    let grant =
        "\"wasi:http/incoming-handler\", \"wasi:keyvalue\", \"wasi:logging\", \"paddock:groups\"";
    let Pair {
        _witness,
        mut n1,
        n2,
        ports,
    } = start_pair(dir.path(), &manifest, grant, |_, _| String::new(), "").await;
    // The primary must see the standby's instance in the group first.
    until("a write reaches the standby", || async {
        write(ports[0]).await.2 == 1
    })
    .await;

    for _ in 0..2 {
        write(ports[0]).await;
    }
    let (primary_count, _) = board(ports[0]).await;
    let (standby_count, _) = board(ports[1]).await;
    let standby_write = write(ports[1]).await.0;
    n1.child.kill().await.unwrap();
    until("node 2 primary", || async { n2.role().await == "primary" }).await;
    // The app hears its role a moment after the node does; until then it
    // refuses writes, which change nothing.
    let mut after = write(ports[1]).await;
    for _ in 0..250 {
        if after.0 != 503 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        after = write(ports[1]).await;
    }

    assert_eq!(standby_count, primary_count);
    assert_eq!(standby_write, 503);
    assert_eq!(after.0, 200);
    assert_eq!(after.1, primary_count + 1, "the count survived the primary");
}

/// One request on its own connection: (status, `location` header).
async fn location(port: u16, method: &str, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let head = raw.split_once("\r\n\r\n").map_or(raw.as_str(), |(h, _)| h);
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let to = head
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("location"))
        })
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    (status, to)
}

/// A standby redirecting to the primary: the board as a singleton on an HA pair (tutorial
/// chapter 2) with `partner_url` on each listener. The standby redirects
/// the board's requests to the primary (307, path and query kept); after
/// the primary is killed, node 2 serves the board itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn board____singleton_on_a_pair_with_partner_url____standby_redirects_to_the_primary() {
    let dir = short_dir();
    let d = dir.path();
    let manifest = d.join("singleton.toml");
    let base = std::fs::read_to_string(root().join("examples/board/paddock.toml")).unwrap();
    std::fs::write(
        &manifest,
        format!("{base}\n[appserver.placement]\nmode = \"singleton\"\n"),
    )
    .unwrap();
    let grant = "\"wasi:http/incoming-handler\", \"wasi:keyvalue\", \"wasi:logging\"";
    let Pair {
        _witness,
        mut n1,
        n2,
        ports,
    } = start_pair(
        d,
        &manifest,
        grant,
        |n, ports| {
            let partner = ports[usize::try_from(2 - n).unwrap()];
            format!("partner_url = \"http://127.0.0.1:{partner}/\"\n")
        },
        "",
    )
    .await;
    board(ports[0]).await;

    let (status, to) = location(ports[1], "POST", "/board?by=2").await;
    let other = location(ports[1], "GET", "/elsewhere").await.0;
    n1.child.kill().await.unwrap();
    until("node 2 primary", || async { n2.role().await == "primary" }).await;
    // The singleton starts a moment after the promotion; `board` waits.
    let (after, _) = board(ports[1]).await;

    assert_eq!(status, 307);
    assert_eq!(to, format!("http://127.0.0.1:{}/board?by=2", ports[0]));
    assert_eq!(other, 404, "no route: not redirected");
    assert_eq!(after, 0, "node 2 serves its own board");
}

/// A standby forwarding to the primary: the board as a singleton on an HA pair with
/// `standby = "forward"` on each listener. The standby's ingress sends the
/// board's requests over the cluster to the primary's board, so either
/// address serves it; after the primary is killed, node 2 serves its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn board____singleton_on_a_pair_with_standby_forward____either_address_serves_the_primary() {
    let dir = short_dir();
    let d = dir.path();
    let manifest = d.join("singleton.toml");
    let base = std::fs::read_to_string(root().join("examples/board/paddock.toml")).unwrap();
    std::fs::write(
        &manifest,
        format!("{base}\n[appserver.placement]\nmode = \"singleton\"\n"),
    )
    .unwrap();
    let grant = "\"wasi:http/incoming-handler\", \"wasi:keyvalue\", \"wasi:logging\"";
    let Pair {
        _witness,
        mut n1,
        n2,
        ports,
    } = start_pair(
        d,
        &manifest,
        grant,
        |_, _| "standby = \"forward\"\n".into(),
        "",
    )
    .await;
    let (_, primary_pid) = board(ports[0]).await;
    // Forwarding starts once the standby hears the primary.
    until("the standby forwards", || async {
        http(ports[1], "GET", "/board")
            .await
            .is_some_and(|r| r.0 == 200)
    })
    .await;

    let via_standby = write(ports[1]).await;
    let (on_primary, _) = board(ports[0]).await;
    let (_, answered_by) = board(ports[1]).await;
    n1.child.kill().await.unwrap();
    until("node 2 primary", || async { n2.role().await == "primary" }).await;
    let (after, own_pid) = board(ports[1]).await;

    assert_eq!(via_standby.0, 200);
    assert_eq!(
        via_standby.1, on_primary,
        "the write landed on the primary's board"
    );
    assert_eq!(answered_by, primary_pid, "the primary's instance answered");
    assert_eq!(after, 0, "node 2 serves its own board");
    assert!(own_pid.starts_with("<2."), "{own_pid}");
}

/// The board on a `cluster` bucket: the bucket itself is
/// replicated between the pair, so the count survives `kill -9` of the
/// primary without the app replicating anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn board____cluster_bucket_primary_killed____count_survives_on_the_standby() {
    let dir = short_dir();
    let manifest = root().join("examples/board/paddock.toml");
    let grant = "\"wasi:http/incoming-handler\", \"wasi:keyvalue\", \"wasi:logging\"";
    let Pair {
        _witness,
        mut n1,
        n2,
        ports,
    } = start_pair(
        dir.path(),
        &manifest,
        grant,
        |_, _| String::new(),
        "scope = \"cluster\"\n",
    )
    .await;

    let mut written = Vec::new();
    for _ in 0..3 {
        until("a write commits", || async {
            write(ports[0]).await.0 == 200
        })
        .await;
        written.push(board(ports[0]).await.0);
    }
    until("the standby's copy has every write", || async {
        board(ports[1]).await.0 == 3
    })
    .await;
    let status = paddock::api::Client::new(&n1.socket)
        .failover()
        .await
        .unwrap();
    n1.child.kill().await.unwrap();
    until("node 2 primary", || async { n2.role().await == "primary" }).await;
    let after_failover = board(ports[1]).await.0;
    until("a write on the new primary commits", || async {
        write(ports[1]).await.0 == 200
    })
    .await;
    let standing = paddock::api::Client::new(&n2.socket)
        .failover()
        .await
        .unwrap();

    assert_eq!(written, [1, 2, 3]);
    let replication = status.replication.unwrap();
    assert_eq!(replication.buckets, ["board"]);
    assert_eq!(replication.partner_matched, Some(replication.last));
    assert_eq!(after_failover, 3, "every committed write survived");
    assert_eq!(board(ports[1]).await.0, 4);
    assert!(standing.partner_outdated, "committed alone once recorded");
    assert!(standing.replication.unwrap().alone);
}
