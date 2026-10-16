//! `examples/tally` through a node (ROADMAP E5): bundled with the CLI and
//! deployed under policy, with a `[datasources.main]` on a scratch Postgres
//! this test starts; its count lives in the database, so it outlives the
//! app. Skipped, saying so, when no Postgres is installed.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

#[path = "../../paddock-host/tests/common/pg.rs"]
mod pg;

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use paddock::node::{LogRing, Node, NodeConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn build() -> PathBuf {
    let target = root().join("target/fixtures/examples-tally");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = StdCommand::new(cargo)
        .current_dir(root())
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .args(["-p", "example-tally", "--target-dir"])
        .arg(&target)
        .status()
        .unwrap()
        .success();
    assert!(ok, "building example-tally failed");
    target.join("wasm32-wasip2/release/example_tally.wasm")
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

/// One request on its own connection: (status, body as JSON).
async fn http(port: u16, method: &str, body: &str) -> (u16, serde_json::Value) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!(
        "{method} /tally HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (
        head.split(' ').nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap_or(serde_json::Value::String(body.to_string())),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tally____on_postgres____counts_in_transactions_and_outlives_the_app() {
    let Some(db) = pg::Scratch::start() else {
        return;
    };
    let wasm = build();
    let dir = tempfile::Builder::new()
        .prefix("pt")
        .tempdir_in("/tmp")
        .unwrap();
    let d = dir.path();
    let socket = d.join("n.sock");
    let (ok, public, err) = paddock(&socket, &["keygen", "--no-password", s(&d.join("k"))]).await;
    assert!(ok, "{err}");
    let bundle = d.join("tally.rbp");
    let manifest = root().join("examples/tally/paddock.toml");
    let (ok, _, err) = paddock(
        &socket,
        &[
            "bundle",
            s(&manifest),
            s(&wasm),
            "--key",
            s(&d.join("k.key")),
            "-o",
            s(&bundle),
        ],
    )
    .await;
    assert!(ok, "{err}");
    std::fs::write(
        d.join("policy.toml"),
        "[apps.tally]\ngrant = [\"wasi:http/incoming-handler\", \"paddock:sql\", \"wasi:logging\"]\n\
         datasources = [\"main\"]\n",
    )
    .unwrap();
    let port = free_port();
    let toml = format!(
        "data_dir = {:?}\nadmin_socket = {socket:?}\ntrusted_keys = [{public:?}]\npolicy = {:?}\n\
         [http.main]\naddress = \"127.0.0.1:{port}\"\n\
         [datasources.main]\nhost = {:?}\nport = {}\ndbname = \"postgres\"\nuser = \"paddock\"\n",
        d.join("data"),
        d.join("policy.toml"),
        db.socket_dir(),
        pg::PORT,
    );
    let node = Arc::new(
        Node::open(NodeConfig::from_toml_str(&toml).unwrap(), LogRing::new(100))
            .await
            .unwrap(),
    );
    let server = node.clone();
    tokio::spawn(async move {
        paddock::node::serve(server, std::future::pending::<()>())
            .await
            .unwrap();
    });
    while !socket.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let (deployed, _, err) = paddock(&socket, &["deploy", s(&bundle)]).await;
    assert!(deployed, "{err}");
    let empty = http(port, "GET", "").await;
    http(port, "POST", "").await;
    http(port, "POST", "").await;
    let posted = http(port, "POST", "5").await;
    let (removed, _, err) = paddock(&socket, &["remove", "tally"]).await;
    assert!(removed, "{err}");
    let (redeployed, _, err) = paddock(&socket, &["deploy", s(&bundle)]).await;
    assert!(redeployed, "{err}");
    let after = http(port, "GET", "").await;

    assert_eq!(empty.0, 200, "{:?}", empty.1);
    assert_eq!(empty.1["count"], 0);
    assert_eq!(posted.0, 200, "{:?}", posted.1);
    assert_eq!(posted.1["count"], 7);
    let recent: Vec<i64> = posted.1["recent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["n"].as_i64().unwrap())
        .collect();
    assert_eq!(
        recent,
        vec![7, 2, 1],
        "each change logged in its transaction"
    );
    assert_eq!(
        after.1["count"], 7,
        "the count is in the database, not the app"
    );
    node.shutdown().await;
}
