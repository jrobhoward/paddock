//! The greeter extension end to end: a node built with it deploys the
//! greeter app, the policy grants it, and the app's calls reach the service.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use example_greeter_service::Greeter;
use minisign::KeyPair;
use paddock::node::{LogRing, Node, NodeConfig};
use paddock_host::Envelope;
use rustbridge_bundle::{BundleBuilder, Manifest, Platform};
use serde_json::json;
use tempfile::TempDir;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// The greeter app, built for `wasm32-wasip2` into `target/fixtures`.
fn app() -> Vec<u8> {
    let target = root().join("target/fixtures/greeter");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(root())
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip2",
            "-p",
            "example-greeter-app",
            "--target-dir",
        ])
        .arg(&target)
        .status()
        .unwrap();
    assert!(status.success(), "building the greeter app failed");
    std::fs::read(target.join("wasm32-wasip2/release/example_greeter_app.wasm")).unwrap()
}

fn bundle(key: &KeyPair, name: &str, component: &[u8], appserver: serde_json::Value) -> Vec<u8> {
    let dir = TempDir::new().unwrap();
    let wasm = dir.path().join("app.wasm");
    std::fs::write(&wasm, component).unwrap();
    let out = dir.path().join("app.rbp");
    BundleBuilder::new(Manifest::new(name, "1.0.0"))
        .add_component(Platform::Wasm32Wasip2, &wasm, "paddock:runtime/app@0.1.0")
        .unwrap()
        .with_extension("appserver", appserver)
        .with_signing_key(key.pk.to_base64(), key.sk.clone())
        .write(&out)
        .unwrap();
    std::fs::read(out).unwrap()
}

async fn ask(node: &Node, app: &str, body: &str) -> String {
    let reply = node
        .call(
            app,
            Envelope::new("text/plain", body),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn greeter____granted_and_not____served_and_denied() {
    let dir = TempDir::new().unwrap();
    let key = KeyPair::generate_unencrypted_keypair().unwrap();
    let policy = dir.path().join("policy.toml");
    std::fs::write(&policy, "[apps.greeted]\ngrant = [\"example:greeting\"]\n").unwrap();
    let config = format!(
        "data_dir = {:?}\nmax_instances = 8\ntrusted_keys = [{:?}]\npolicy = {:?}\n",
        dir.path().join("data"),
        key.pk.to_base64(),
        policy
    );
    let node = Node::open_with(
        NodeConfig::from_toml_str(&config).unwrap(),
        LogRing::new(100),
        vec![Arc::new(Greeter::default())],
    )
    .await
    .unwrap();
    let component = app();
    let declared = json!({ "capabilities": { "extensions": { "example:greeting": { "salutation": "Howdy" } } } });

    let granted = node
        .deploy(&bundle(&key, "greeted", &component, declared.clone()))
        .await
        .unwrap();
    let denied = node
        .deploy(&bundle(&key, "ungranted", &component, declared))
        .await
        .unwrap();
    let undeclared = node
        .deploy(&bundle(&key, "undeclared", &component, json!({})))
        .await;
    let hello = ask(&node, "greeted", "hello:Ada").await;
    let refused = ask(&node, "ungranted", "hello:Ada").await;
    let count = ask(&node, "greeted", "count").await;

    assert_eq!(granted.granted, vec!["example:greeting"]);
    assert_eq!(denied.denied, vec!["example:greeting"]);
    let err = undeclared.unwrap_err().to_string();
    assert!(
        err.contains("needs extensions.\"example:greeting\""),
        "{err}"
    );
    assert_eq!(hello, "Howdy, Ada (from greeted)");
    assert_eq!(refused, "err:access-denied");
    assert_eq!(count, "1");
    node.shutdown().await;
}
