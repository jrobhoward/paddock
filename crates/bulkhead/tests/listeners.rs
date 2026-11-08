//! Connection-handler apps on a node: listeners bound from node config,
//! deploy, upgrade, rollback, stop, start, restart and remove; for the
//! component (an instance per connection) and, on Linux, the same fixture
//! built as a native program, whose instances take the connections
//! (ROADMAP Z8g).

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use bulkhead::api::RolloutParams;
use common::{TestNode, kind, native_bundle, native_fixtures};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn bound(port: u16) -> String {
    format!("[listeners.lines]\naddress = \"127.0.0.1:{port}\"\n")
}

/// The idle timeout is long because a connection held open across an
/// upgrade sits idle while the new version compiles, which under a loaded
/// debug test run can take several seconds.
/// The connection fixture as a native program, v1 or v2, once per test
/// binary.
fn native_conn(version: u8) -> Vec<u8> {
    static BUILT: [OnceLock<PathBuf>; 2] = [OnceLock::new(), OnceLock::new()];
    let path = BUILT[usize::from(version - 1)].get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = root.join(format!("target/fixtures/native-conn-v{version}"));
        let features = if version > 1 { "native,v2" } else { "native" };
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = std::process::Command::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "bulkhead-fixture-conn",
                "--features",
                features,
            ])
            .args(["--bin", "bulkhead-fixture-conn", "--target-dir"])
            .arg(&target)
            .status()
            .unwrap()
            .success();
        assert!(ok, "building the native connection fixture failed");
        target.join("debug/bulkhead-fixture-conn")
    });
    std::fs::read(path).unwrap()
}

fn native_manifest(listeners: serde_json::Value) -> serde_json::Value {
    json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 2 },
        "listeners": listeners
    })
}

fn native_node(port: u16) -> String {
    format!(
        "{}[native]\nrunner = {:?}\n",
        bound(port),
        native_fixtures().0
    )
}

/// A line connection.
struct Line {
    read: BufReader<tokio::net::tcp::OwnedReadHalf>,
    write: tokio::net::tcp::OwnedWriteHalf,
}

impl Line {
    async fn open(port: u16) -> Option<Self> {
        let (read, write) = TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?
            .into_split();
        Some(Self {
            read: BufReader::new(read),
            write,
        })
    }

    async fn ask(&mut self, line: &str) -> Option<String> {
        self.write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .ok()?;
        let mut s = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), self.read.read_line(&mut s))
            .await
            .ok()?
            .ok()?;
        (n > 0).then(|| s.trim_end().to_string())
    }
}

/// One request on a new connection; `None` if nothing answered.
async fn ask(port: u16, line: &str) -> Option<String> {
    Line::open(port).await?.ask(line).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_app____listener_not_bound____deploy_refused() {
    if !cfg!(target_os = "linux") {
        eprintln!("not Linux: native apps do not run here");
        return;
    }
    let mut n = TestNode::start_native("").await;
    let listeners = json!([{ "name": "lines", "idle_timeout_ms": 60_000 }]);

    let refused = n
        .client
        .deploy(native_bundle(
            &n.key,
            "lines",
            "1.0.0",
            &native_conn(1),
            native_manifest(listeners),
        ))
        .await
        .unwrap_err();
    let list = n.client.list().await.unwrap();

    assert_eq!(kind(&refused), "invalid");
    assert!(
        refused
            .to_string()
            .contains("listener 'lines' is not bound"),
        "{refused}"
    );
    assert!(list.is_empty());
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_app____lifecycle____served_upgraded_rolled_back_stopped_restored() {
    if !cfg!(target_os = "linux") {
        eprintln!("not Linux: native apps do not run here");
        return;
    }
    let port = free_port();
    let mut n = TestNode::start(&native_node(port)).await;
    let listeners = json!([{ "name": "lines", "idle_timeout_ms": 60_000 }]);
    let v1 = native_bundle(
        &n.key,
        "lines",
        "1.0.0",
        &native_conn(1),
        native_manifest(listeners.clone()),
    );
    let deployed = n.client.deploy(v1).await.unwrap();
    let first = ask(port, "echo one").await;
    let (mut a, mut b) = (
        Line::open(port).await.unwrap(),
        Line::open(port).await.unwrap(),
    );
    let pids = (a.ask("self").await, b.ask("self").await);
    let status = n.client.status("lines").await.unwrap();

    let mut held = Line::open(port).await.unwrap();
    held.ask("echo before").await;
    let v2 = native_bundle(
        &n.key,
        "lines",
        "2.0.0",
        &native_conn(2),
        native_manifest(listeners),
    );
    let up = n
        .client
        .upgrade("lines", v2, &RolloutParams::default())
        .await
        .unwrap();
    let after_up = ask(port, "echo x").await;
    let held_after_up = held.ask("echo x").await;
    let back = n
        .client
        .rollback("lines", &RolloutParams::default())
        .await
        .unwrap();
    let after_back = ask(port, "echo x").await;

    n.client.stop("lines").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let while_stopped = ask(port, "echo x").await;
    n.client.start("lines").await.unwrap();
    let started = ask(port, "echo started").await;
    n.restart().await;
    let restored = ask(port, "echo restored").await;
    n.client.remove("lines").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let removed = ask(port, "echo x").await;

    assert_eq!(deployed.app.status, "running");
    assert_eq!(first.as_deref(), Some("one"));
    assert_ne!(pids.0, pids.1, "spread over the pool: {pids:?}");
    let listener = &status.listeners[0];
    assert_eq!(listener.name, "lines");
    assert_eq!(listener.address, format!("127.0.0.1:{port}"));
    assert!(listener.accepted >= 3, "{status:?}");
    assert_eq!(status.instance_list.len(), 2, "{status:?}");
    assert_eq!((up.from.as_str(), up.to.as_str()), ("1.0.0", "2.0.0"));
    assert_eq!(after_up.as_deref(), Some("v2:x"));
    assert_eq!(
        held_after_up, None,
        "a restart upgrade closes open connections"
    );
    assert_eq!((back.from.as_str(), back.to.as_str()), ("2.0.0", "1.0.0"));
    assert_eq!(after_back.as_deref(), Some("x"));
    assert_eq!(while_stopped, None);
    assert_eq!(started.as_deref(), Some("started"));
    assert_eq!(restored.as_deref(), Some("restored"));
    assert_eq!(removed, None);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_app____upgrade_changes_listeners____refused() {
    if !cfg!(target_os = "linux") {
        eprintln!("not Linux: native apps do not run here");
        return;
    }
    let port = free_port();
    let mut n = TestNode::start(&native_node(port)).await;
    let lines = json!([{ "name": "lines" }]);
    let more = json!([{ "name": "lines", "max_connections": 8 }]);
    let v1 = native_bundle(
        &n.key,
        "lines",
        "1.0.0",
        &native_conn(1),
        native_manifest(lines),
    );
    n.client.deploy(v1).await.unwrap();

    let v2 = native_bundle(
        &n.key,
        "lines",
        "2.0.0",
        &native_conn(2),
        native_manifest(more),
    );
    let refused = n
        .client
        .upgrade("lines", v2, &RolloutParams::default())
        .await
        .unwrap_err();
    let still = ask(port, "echo x").await;

    assert_eq!(kind(&refused), "invalid");
    assert!(
        refused.to_string().contains("listeners change"),
        "{refused}"
    );
    assert_eq!(still.as_deref(), Some("x"));
    n.shutdown().await;
}
