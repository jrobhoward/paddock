//! Connection-handler apps on a node: listeners bound from node config,
//! deploy, upgrade, rollback, stop, start, restart and remove.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{TestNode, conn, conn_bundle, kind};
use paddock::api::RolloutParams;
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
fn manifest() -> serde_json::Value {
    json!({ "listeners": [{ "name": "lines", "idle_timeout_ms": 60_000 }] })
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
async fn connection_app____lifecycle____served_upgraded_rolled_back_stopped_restored() {
    let port = free_port();
    let mut n = TestNode::start(&bound(port)).await;
    let deployed = n
        .client
        .deploy(conn_bundle(&n.key, "lines", "1.0.0", &conn(1), manifest()))
        .await
        .unwrap();
    let first = ask(port, "echo one").await;
    let status = n.client.status("lines").await.unwrap();

    let mut held = Line::open(port).await.unwrap();
    held.ask("echo before").await;
    let up = n
        .client
        .upgrade(
            "lines",
            conn_bundle(&n.key, "lines", "2.0.0", &conn(2), manifest()),
            &RolloutParams::default(),
        )
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
    let listener = &status.listeners[0];
    assert_eq!(listener.name, "lines");
    assert_eq!(listener.address, format!("127.0.0.1:{port}"));
    assert!(listener.accepted >= 1, "{status:?}");
    assert_eq!(
        (up.from.as_str(), up.to.as_str(), up.mode.as_str()),
        ("1.0.0", "2.0.0", "new-connections")
    );
    assert_eq!(after_up.as_deref(), Some("v2:x"));
    assert_eq!(held_after_up.as_deref(), Some("x"));
    assert_eq!((back.from.as_str(), back.to.as_str()), ("2.0.0", "1.0.0"));
    assert_eq!(after_back.as_deref(), Some("x"));
    assert_eq!(while_stopped, None);
    assert_eq!(started.as_deref(), Some("started"));
    assert_eq!(restored.as_deref(), Some("restored"));
    assert_eq!(removed, None);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_app____listener_not_bound____deploy_refused() {
    let mut n = TestNode::start("").await;

    let refused = n
        .client
        .deploy(conn_bundle(&n.key, "lines", "1.0.0", &conn(1), manifest()))
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
