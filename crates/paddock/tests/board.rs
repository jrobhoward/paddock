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
