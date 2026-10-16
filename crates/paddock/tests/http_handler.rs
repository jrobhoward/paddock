//! HTTP-handler apps on a node (ROADMAP E6): a `wasi:http/proxy` component
//! deployed under operator policy, an instance per request on its routes;
//! upgrade, rollback, stop, start, restart and remove.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{TestNode, http_handler, http_handler_bundle};
use paddock::api::RolloutParams;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A node with an ingress on `port` and a policy granting `web` its routes.
async fn node(port: u16) -> (TestNode, tempfile::TempDir) {
    let policy_dir = tempfile::TempDir::new().unwrap();
    let policy = policy_dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.web]\ngrant = [\"wasi:http/incoming-handler\"]\n",
    )
    .unwrap();
    let extra = format!("policy = {policy:?}\n[http.main]\naddress = \"127.0.0.1:{port}\"\n");
    (TestNode::start(&extra).await, policy_dir)
}

fn manifest() -> serde_json::Value {
    json!({ "capabilities": { "wasi:http/incoming-handler": {
        "routes": [{ "path_prefix": "/hello" }, { "path_prefix": "/self" }]
    } } })
}

/// One GET on its own connection: (status, body); status 0 if nothing
/// answered.
async fn get(port: u16, path: &str) -> (u16, String) {
    let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return (0, String::new());
    };
    let req = format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw).await;
    let Some((head, body)) = raw.split_once("\r\n\r\n") else {
        return (0, String::new());
    };
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    // A streamed body arrives chunked: one chunk, then the last.
    let body = match body.split_once("\r\n") {
        Some((size, rest)) if head.to_ascii_lowercase().contains("chunked") => {
            let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            rest[..n.min(rest.len())].to_string()
        }
        _ => body.to_string(),
    };
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_handler____lifecycle____served_upgraded_rolled_back_stopped_restored() {
    let port = free_port();
    let (mut n, _policy) = node(port).await;
    let deployed = n
        .client
        .deploy(http_handler_bundle(
            &n.key,
            "web",
            "1.0.0",
            &http_handler(1),
            manifest(),
        ))
        .await
        .unwrap();
    let first = get(port, "/hello").await;
    let (a, b) = (get(port, "/self").await, get(port, "/self").await);
    let unrouted = get(port, "/echo").await;
    let status = n.client.status("web").await.unwrap();

    let up = n
        .client
        .upgrade(
            "web",
            http_handler_bundle(&n.key, "web", "2.0.0", &http_handler(2), manifest()),
            &RolloutParams::default(),
        )
        .await
        .unwrap();
    let after_up = get(port, "/hello").await;
    let back = n
        .client
        .rollback("web", &RolloutParams::default())
        .await
        .unwrap();
    let after_back = get(port, "/hello").await;
    n.client.stop("web").await.unwrap();
    let while_stopped = get(port, "/hello").await;
    n.client.start("web").await.unwrap();
    let started = get(port, "/hello").await;
    n.restart().await;
    let restored = get(port, "/hello").await;
    n.client.remove("web").await.unwrap();
    let removed = get(port, "/hello").await;

    assert_eq!(deployed.app.status, "running");
    assert_eq!(first, (200, "hello".to_string()));
    assert_eq!((a.0, b.0), (200, 200));
    assert_ne!(a.1, b.1, "an instance per request");
    assert_eq!(unrouted.0, 404);
    assert_eq!(status.summary.status, "running");
    assert_eq!(
        (up.from.as_str(), up.to.as_str(), up.mode.as_str()),
        ("1.0.0", "2.0.0", "new-requests")
    );
    assert_eq!(after_up, (200, "hello v2".to_string()));
    assert_eq!((back.from.as_str(), back.to.as_str()), ("2.0.0", "1.0.0"));
    assert_eq!(after_back, (200, "hello".to_string()));
    assert_eq!(while_stopped.0, 404);
    assert_eq!(started, (200, "hello".to_string()));
    assert_eq!(restored, (200, "hello".to_string()));
    assert_eq!(removed.0, 404);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_handler____routes_not_granted____deployed_and_unrouted() {
    let port = free_port();
    let (mut n, _policy) = node(port).await;

    let deployed = n
        .client
        .deploy(http_handler_bundle(
            &n.key,
            "ungranted",
            "1.0.0",
            &http_handler(1),
            manifest(),
        ))
        .await
        .unwrap();
    let got = get(port, "/hello").await;

    assert!(
        deployed
            .denied
            .iter()
            .any(|d| d == "wasi:http/incoming-handler"),
        "{deployed:?}"
    );
    assert_eq!(got.0, 404);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_handler____node_without_ingress____deploy_refused() {
    let policy_dir = tempfile::TempDir::new().unwrap();
    let policy = policy_dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.web]\ngrant = [\"wasi:http/incoming-handler\"]\n",
    )
    .unwrap();
    let mut n = TestNode::start(&format!("policy = {policy:?}\n")).await;

    let refused = n
        .client
        .deploy(http_handler_bundle(
            &n.key,
            "web",
            "1.0.0",
            &http_handler(1),
            manifest(),
        ))
        .await;

    let err = refused.unwrap_err().to_string();
    assert!(err.contains("no HTTP ingress"), "{err}");
    tokio::time::sleep(Duration::from_millis(10)).await;
    n.shutdown().await;
}
