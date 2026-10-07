//! The `paddock` binary against an in-process node.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Arc, OnceLock};

use minisign::KeyPair;
use paddock::node::{LogRing, Node, NodeConfig};
use rustbridge_bundle::{BundleBuilder, Manifest, Platform};
use tempfile::TempDir;

fn chaos() -> Vec<u8> {
    static BUILT: OnceLock<()> = OnceLock::new();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = root.join("target/fixtures/chaos-plain");
    BUILT.get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = StdCommand::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "-p",
                "paddock-fixture-chaos",
                "--target-dir",
            ])
            .arg(&target)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    });
    std::fs::read(target.join("wasm32-wasip2/release/paddock_fixture_chaos.wasm")).unwrap()
}

fn write_bundle(dir: &Path, key: &KeyPair, name: &str) -> PathBuf {
    let wasm = dir.join("app.wasm");
    std::fs::write(&wasm, chaos()).unwrap();
    let out = dir.join(format!("{name}.rbp"));
    BundleBuilder::new(Manifest::new(name, "1.0.0"))
        .add_component(Platform::Wasm32Wasip2, &wasm, "paddock:runtime/app@0.1.0")
        .unwrap()
        .with_extension(
            "appserver",
            serde_json::json!({ "lifecycle": { "shutdown_timeout_ms": 300 } }),
        )
        .with_signing_key(key.pk.to_base64(), key.sk.clone())
        .write(&out)
        .unwrap();
    out
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn paddock(socket: &Path, args: &[&str]) -> Output {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .await
        .unwrap();
    Output {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli____full_cycle____deploy_list_status_call_kill_stop_start_remove() {
    let dir = TempDir::new().unwrap();
    let key = KeyPair::generate_unencrypted_keypair().unwrap();
    let toml = format!(
        "data_dir = {:?}\nmax_instances = 8\ntrusted_keys = [{:?}]\n",
        dir.path().join("data"),
        key.pk.to_base64()
    );
    let node = Arc::new(
        Node::open(NodeConfig::from_toml_str(&toml).unwrap(), LogRing::new(100))
            .await
            .unwrap(),
    );
    let socket = node.socket();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn({
        let node = node.clone();
        async move {
            paddock::node::serve(node, async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
        }
    });
    while !socket.exists() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let bundle = write_bundle(dir.path(), &key, "demo");

    let deploy = paddock(&socket, &["deploy", bundle.to_str().unwrap()]).await;
    let list = paddock(&socket, &["list"]).await;
    let list_json = paddock(&socket, &["--json", "list"]).await;
    let status = paddock(&socket, &["status", "demo"]).await;
    let called = paddock(&socket, &["call", "demo", "echo:hi there"]).await;
    let cast = paddock(&socket, &["cast", "demo", "count"]).await;
    let counted = paddock(&socket, &["call", "demo", "count"]).await;
    let ps_table = paddock(&socket, &["ps"]).await;
    let ps_json = paddock(&socket, &["--json", "ps"]).await;
    let ps: Vec<paddock::api::InstanceInfo> = serde_json::from_str(&ps_json.stdout).unwrap();
    let state = paddock(&socket, &["state", &ps[0].pid]).await;
    let debug = paddock(&socket, &["log-level", "demo", "debug"]).await;
    let bogus = paddock(&socket, &["log-level", "demo", "loud"]).await;
    let reset = paddock(&socket, &["log-level", "demo", "reset"]).await;
    let no_previous = paddock(&socket, &["rollback", "demo", "--restart"]).await;
    let kill = paddock(&socket, &["kill", &ps[0].pid]).await;
    let stopped_app = paddock(&socket, &["stop", "demo"]).await;
    let started = paddock(&socket, &["start", "demo"]).await;
    let removed = paddock(&socket, &["remove", "demo"]).await;
    let missing = paddock(&socket, &["status", "demo"]).await;

    assert!(deploy.ok, "{}", deploy.stderr);
    assert_eq!(deploy.stdout.trim(), "deployed demo 1.0.0 (running)");
    assert!(
        list.stdout
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("demo  1.0.0    running  1"),
        "{}",
        list.stdout
    );
    let apps: Vec<paddock::api::AppSummary> = serde_json::from_str(&list_json.stdout).unwrap();
    assert_eq!(apps[0].name, "demo");
    assert!(
        status.stdout.contains("status:    running"),
        "{}",
        status.stdout
    );
    assert_eq!(called.stdout.trim(), "hi there");
    assert_eq!(cast.stdout.trim(), "cast to demo");
    assert_eq!(counted.stdout.trim(), "2");
    assert!(
        ps_table
            .stdout
            .lines()
            .next()
            .unwrap()
            .ends_with("QUEUED  DROPPED"),
        "{}",
        ps_table.stdout
    );
    assert_eq!(
        state.stdout,
        format!("{} (raw, 8 bytes)\n0200000000000000\n", ps[0].pid)
    );
    assert_eq!(debug.stdout.trim(), "demo logs at debug");
    assert!(
        !bogus.ok && bogus.stderr.contains("unknown log level 'loud'"),
        "{}",
        bogus.stderr
    );
    assert_eq!(reset.stdout.trim(), "demo logs at its manifest level");
    assert!(
        !no_previous.ok && no_previous.stderr.contains("demo has no previous version"),
        "{}",
        no_previous.stderr
    );
    assert!(
        kill.ok && kill.stdout.starts_with("killed <"),
        "{}",
        kill.stdout
    );
    assert_eq!(stopped_app.stdout.trim(), "demo stopped");
    assert_eq!(started.stdout.trim(), "demo started");
    assert_eq!(removed.stdout.trim(), "demo removed");
    assert!(!missing.ok);
    assert!(
        missing.stderr.contains("demo is not deployed (not_found)"),
        "{}",
        missing.stderr
    );

    node.shutdown().await;
    let _ = stop.send(());
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli____no_node_listening____clear_error() {
    let dir = TempDir::new().unwrap();

    let out = paddock(&dir.path().join("nothing.sock"), &["list"]).await;

    assert!(!out.ok);
    assert!(
        out.stderr.contains("cannot reach the node"),
        "{}",
        out.stderr
    );
}
