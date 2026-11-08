//! The greeter extension end to end: a node built with it deploys the
//! greeter app, the policy grants it, and the app's calls reach the
//! service; the app built as a native program.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bulkhead::node::{LogRing, Node, NodeConfig};
use bulkhead_host::Envelope;
use example_greeter_service::Greeter;
use minisign::KeyPair;
use rustbridge_bundle::{BundleBuilder, Manifest, Platform};
use serde_json::json;
use tempfile::TempDir;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn cargo(target: &Path, args: &[&str]) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(root())
        .arg("build")
        .args(args)
        .arg("--target-dir")
        .arg(target)
        .status()
        .unwrap();
    assert!(status.success(), "building {args:?} failed");
}

/// The greeter app as a native program and `bulkhead-runner`, built once
/// per test binary.
fn native() -> &'static (PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let target = root().join("target/fixtures/greeter-native");
        cargo(
            &target,
            &[
                "-p",
                "bulkhead-native",
                "-p",
                "example-greeter-app",
                "--features",
                "example-greeter-app/native",
            ],
        );
        let debug = target.join("debug");
        (
            debug.join("bulkhead-runner"),
            debug.join("example-greeter-app"),
        )
    })
}

/// A signed bundle of the app `name`, with `appserver`.
fn bundle(key: &KeyPair, name: &str, appserver: serde_json::Value) -> Vec<u8> {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("app.rbp");
    let mut builder = BundleBuilder::new(Manifest::new(name, "1.0.0"));
    let mut appserver = appserver;
    let platform = Platform::current().unwrap();
    let program = dir.path().join(platform.as_str()).join(name);
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    std::fs::copy(&native().1, &program).unwrap();
    builder = builder.add_library(platform, &program).unwrap();
    let fields = appserver.as_object_mut().unwrap();
    fields.insert("kind".into(), json!("native-process"));
    fields.insert("native".into(), json!({ "program": "executable" }));
    builder
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
        "data_dir = {:?}\nmax_instances = 8\ntrusted_keys = [{:?}]\npolicy = {:?}\n\
         [native]\nrunner = {:?}\n",
        dir.path().join("data"),
        key.pk.to_base64(),
        policy,
        native().0
    );
    let node = Node::open_with(
        NodeConfig::from_toml_str(&config).unwrap(),
        LogRing::new(100),
        vec![Arc::new(Greeter::default())],
    )
    .await
    .unwrap();
    let declared = json!({ "capabilities": { "extensions": { "example:greeting": { "salutation": "Howdy" } } } });

    let granted = node
        .deploy(&bundle(&key, "greeted", declared.clone()))
        .await
        .unwrap();
    let denied = node
        .deploy(&bundle(&key, "ungranted", declared))
        .await
        .unwrap();
    // A native program imports nothing: undeclared is ungranted.
    node.deploy(&bundle(&key, "undeclared", json!({})))
        .await
        .unwrap();
    let undeclared = ask(&node, "undeclared", "hello:Ada").await;
    let hello = ask(&node, "greeted", "hello:Ada").await;
    let refused = ask(&node, "ungranted", "hello:Ada").await;
    let count = ask(&node, "greeted", "count").await;

    assert_eq!(granted.granted, vec!["example:greeting"]);
    assert_eq!(denied.denied, vec!["example:greeting"]);
    assert_eq!(undeclared, "err:access-denied");
    assert_eq!(hello, "Howdy, Ada (from greeted)");
    assert_eq!(refused, "err:access-denied");
    assert_eq!(count, "1");
    node.shutdown().await;
}
