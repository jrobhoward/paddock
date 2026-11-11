//! The `bulkhead` binary against an in-process node.

// Native apps run on Linux only.
#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bulkhead::node::{LogRing, Node, NodeConfig};
use minisign::KeyPair;
use tempfile::TempDir;

fn write_bundle(dir: &Path, key: &KeyPair, name: &str) -> PathBuf {
    let out = dir.join(format!("{name}.rbp"));
    let bytes = common::app_bundle(
        key,
        name,
        "1.0.0",
        &common::native_conformance(),
        serde_json::json!({
            "lifecycle": { "shutdown_timeout_ms": 300 },
            "registered_names": ["demo-name"]
        }),
    );
    std::fs::write(&out, bytes).unwrap();
    out
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn bulkhead(socket: &Path, args: &[&str]) -> Output {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_bulkhead"))
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
        "data_dir = {:?}\nmax_instances = 8\ntrusted_keys = [{:?}]\n[native]\nrunner = {:?}\n",
        dir.path().join("data"),
        key.pk.to_base64(),
        common::native_fixtures().0
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
            bulkhead::node::serve(node, async {
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

    let deploy = bulkhead(&socket, &["deploy", bundle.to_str().unwrap()]).await;
    let list = bulkhead(&socket, &["list"]).await;
    let list_json = bulkhead(&socket, &["--json", "list"]).await;
    let status = bulkhead(&socket, &["status", "demo"]).await;
    let called = bulkhead(&socket, &["call", "demo", "echo:hi there"]).await;
    let cast = bulkhead(&socket, &["cast", "demo", "count"]).await;
    let counted = bulkhead(&socket, &["call", "demo", "count"]).await;
    let ps_table = bulkhead(&socket, &["ps"]).await;
    let ps_json = bulkhead(&socket, &["--json", "ps"]).await;
    let ps: Vec<bulkhead::api::InstanceInfo> = serde_json::from_str(&ps_json.stdout).unwrap();
    let state = bulkhead(&socket, &["state", &ps[0].pid]).await;
    let by_pid = bulkhead(&socket, &["call", "--pid", &ps[0].pid, "echo:by-pid"]).await;
    let registered = bulkhead(&socket, &["call", "demo", "register:demo-name"]).await;
    let by_name = bulkhead(&socket, &["call", "--name", "demo-name", "echo:by-name"]).await;
    let cast_by_name = bulkhead(&socket, &["cast", "--name", "demo-name", "count"]).await;
    let too_many = bulkhead(&socket, &["call", "--pid", &ps[0].pid, "demo", "x"]).await;
    let unknown_name = bulkhead(&socket, &["call", "--name", "nobody", "x"]).await;
    let debug = bulkhead(&socket, &["log-level", "demo", "debug"]).await;
    let bogus = bulkhead(&socket, &["log-level", "demo", "loud"]).await;
    let reset = bulkhead(&socket, &["log-level", "demo", "reset"]).await;
    let no_previous = bulkhead(&socket, &["rollback", "demo", "--restart"]).await;
    let kill = bulkhead(&socket, &["kill", &ps[0].pid]).await;
    let stopped_app = bulkhead(&socket, &["stop", "demo"]).await;
    let started = bulkhead(&socket, &["start", "demo"]).await;
    let removed = bulkhead(&socket, &["remove", "demo"]).await;
    let missing = bulkhead(&socket, &["status", "demo"]).await;

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
    let apps: Vec<bulkhead::api::AppSummary> = serde_json::from_str(&list_json.stdout).unwrap();
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
            .ends_with("RESTARTS  LAST EXIT"),
        "{}",
        ps_table.stdout
    );
    assert!(
        state
            .stdout
            .starts_with(&format!("{} (bulkhead-sdk, ", ps[0].pid)),
        "{}",
        state.stdout
    );
    assert!(state.stdout.contains("\"count\": 2"), "{}", state.stdout);
    assert_eq!(by_pid.stdout.trim(), "by-pid");
    assert_eq!(registered.stdout.trim(), "ok");
    assert_eq!(by_name.stdout.trim(), "by-name");
    assert_eq!(cast_by_name.stdout.trim(), "cast to name demo-name");
    assert!(
        !too_many.ok && too_many.stderr.contains("give only the body"),
        "{}",
        too_many.stderr
    );
    assert!(
        !unknown_name.ok
            && unknown_name
                .stderr
                .contains("no instance registered as nobody"),
        "{}",
        unknown_name.stderr
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

    let out = bulkhead(&dir.path().join("nothing.sock"), &["list"]).await;

    assert!(!out.ok);
    assert!(
        out.stderr.contains("cannot reach the node"),
        "{}",
        out.stderr
    );
}

/// Run `bulkhead --socket SOCKET shell` with `input` on stdin, its config
/// directory under `home`.
async fn shell(socket: &Path, home: &Path, input: &str) -> Output {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bulkhead"))
        .arg("--socket")
        .arg(socket)
        .arg("shell")
        .env("XDG_CONFIG_HOME", home)
        .env_remove("BULKHEAD_CONTACTS")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).await.unwrap();
    drop(stdin);
    let out = child.wait_with_output().await.unwrap();
    Output {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell____lines_on_stdin____each_runs_and_errors_do_not_end_the_session() {
    let dir = TempDir::new().unwrap();
    let key = KeyPair::generate_unencrypted_keypair().unwrap();
    let toml = format!(
        "data_dir = {:?}\nmax_instances = 8\ntrusted_keys = [{:?}]\n[native]\nrunner = {:?}\n",
        dir.path().join("data"),
        key.pk.to_base64(),
        common::native_fixtures().0
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
            bulkhead::node::serve(node, async {
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
    let home = dir.path().join("home");

    let out = shell(
        &socket,
        &home,
        &format!(
            "deploy {}\ncall demo 'echo:hi there'\nconnect nobody\nnode -c x\nbogus\nls --json\nconnect local\nexit\nps\n",
            bundle.display()
        ),
    )
    .await;

    assert!(out.ok, "{}", out.stderr);
    assert!(
        out.stdout.contains("deployed demo 1.0.0 (running)"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("hi there"), "{}", out.stdout);
    assert!(out.stdout.contains("\"name\": \"demo\""), "{}", out.stdout);
    assert!(out.stdout.contains("connected to local"), "{}", out.stdout);
    assert!(
        !out.stdout.contains("PID"),
        "nothing runs after exit: {}",
        out.stdout
    );
    assert!(out.stderr.contains("no contact nobody"), "{}", out.stderr);
    assert!(out.stderr.contains("outside the shell"), "{}", out.stderr);
    assert!(out.stderr.contains("bogus"), "{}", out.stderr);
    node.shutdown().await;
    let _ = stop.send(());
    server.await.unwrap();
}

/// Start `bulkhead node` on a config with `extra`, and answer what
/// another process of the same user finds: whether it may read the node's
/// environment, and the node's `NoNewPrivs` line.
#[cfg(target_os = "linux")]
async fn node_process_fence(extra: &str) -> (bool, String) {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("node.toml");
    std::fs::write(
        &config,
        format!("data_dir = {:?}\n{extra}", dir.path().join("data")),
    )
    .unwrap();
    let log_path = dir.path().join("node.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bulkhead"))
        .args(["node", "--config"])
        .arg(&config)
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    common::wait_for_socket(&mut child, &dir.path().join("data/admin.sock"), &log_path).await;
    let proc = PathBuf::from(format!("/proc/{}", child.id().unwrap()));

    let environ = std::fs::read(proc.join("environ")).is_ok();
    let status = std::fs::read_to_string(proc.join("status")).unwrap();
    let no_new_privs = status
        .lines()
        .find(|l| l.starts_with("NoNewPrivs:"))
        .unwrap_or_default()
        .to_string();

    child.kill().await.unwrap();
    (environ, no_new_privs)
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node____started____not_dumpable_and_no_new_privs() {
    let (environ_readable, no_new_privs) = node_process_fence("").await;

    assert!(!environ_readable, "the node's environment was readable");
    assert_eq!(no_new_privs, "NoNewPrivs:\t1");
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node____debuggable____dumpable_and_no_new_privs() {
    let (environ_readable, no_new_privs) = node_process_fence("debuggable = true\n").await;

    assert!(environ_readable, "the node's environment was not readable");
    assert_eq!(no_new_privs, "NoNewPrivs:\t1");
}
