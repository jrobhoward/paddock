//! The SDK examples end to end: built for wasm32-wasip2, keyed and bundled
//! with the `paddock` binary, deployed to a node, and driven through the
//! CLI.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use paddock::node::{LogRing, Node, NodeConfig};
use tempfile::TempDir;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Build `packages` together into `target/fixtures/examples-<tag>` (one
/// directory per feature set, since features unify within a build) and
/// return the directory holding the `.wasm` files.
fn build(packages: &[&str], features: Option<&str>, tag: &str) -> PathBuf {
    let target = root().join(format!("target/fixtures/examples-{tag}"));
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = StdCommand::new(cargo);
    cmd.current_dir(root())
        .args(["build", "--release", "--target", "wasm32-wasip2"]);
    for p in packages {
        cmd.args(["-p", p]);
    }
    cmd.arg("--target-dir").arg(&target);
    if let Some(f) = features {
        cmd.args(["--features", f]);
    }
    assert!(
        cmd.status().unwrap().success(),
        "building {packages:?} failed"
    );
    target.join("wasm32-wasip2/release")
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn paddock(socket: &Path, args: &[&str]) -> Run {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
        .env("PADDOCK_KEY_PASSWORD", "test-password")
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .await
        .unwrap();
    Run {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn examples____bundled_with_the_cli____run_upgrade_and_fail_as_designed() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("a.sock");
    let ex = root().join("examples");
    let plain = build(
        &[
            "example-counter",
            "example-ping-pong",
            "example-chaos",
            "example-line-server",
        ],
        None,
        "plain",
    );
    let counter = [
        plain.join("example_counter.wasm"),
        build(
            &["example-counter"],
            Some("example-counter/v2"),
            "counter-v2",
        )
        .join("example_counter.wasm"),
        build(
            &["example-counter"],
            Some("example-counter/v3"),
            "counter-v3",
        )
        .join("example_counter.wasm"),
    ];
    let ping_pong = plain.join("example_ping_pong.wasm");
    let plain_wasm = plain.clone();
    let chaos = plain.join("example_chaos.wasm");

    // One encrypted key (opened with PADDOCK_KEY_PASSWORD) for the first
    // bundle; a plain key for the rest, since key derivation is slow.
    let encrypted = paddock(&socket, &["keygen", s(&dir.path().join("signer"))]).await;
    let plain = paddock(
        &socket,
        &["keygen", "--no-password", s(&dir.path().join("fast"))],
    )
    .await;
    assert!(
        encrypted.ok && plain.ok,
        "{} {}",
        encrypted.stderr,
        plain.stderr
    );
    let encrypted_key = dir.path().join("signer.key");
    let plain_key = dir.path().join("fast.key");
    let bundle_with = |key_file: PathBuf, manifest: PathBuf, wasm: PathBuf, out: &str| {
        let out = dir.path().join(out);
        let socket = socket.clone();
        async move {
            let r = paddock(
                &socket,
                &[
                    "bundle",
                    s(&manifest),
                    s(&wasm),
                    "--key",
                    s(&key_file),
                    "-o",
                    s(&out),
                ],
            )
            .await;
            assert!(r.ok, "bundle {}: {}", manifest.display(), r.stderr);
            out
        }
    };
    let bundle = |manifest: PathBuf, wasm: PathBuf, out: &'static str| {
        bundle_with(plain_key.clone(), manifest, wasm, out)
    };
    let c1 = bundle_with(
        encrypted_key.clone(),
        ex.join("counter/paddock.toml"),
        counter[0].clone(),
        "c1.rbp",
    )
    .await;
    let c2 = bundle(
        ex.join("counter/paddock-v2.toml"),
        counter[1].clone(),
        "c2.rbp",
    )
    .await;
    let c3 = bundle(
        ex.join("counter/paddock-v3.toml"),
        counter[2].clone(),
        "c3.rbp",
    )
    .await;
    let pong = bundle(
        ex.join("ping-pong/pong.toml"),
        ping_pong.clone(),
        "pong.rbp",
    )
    .await;
    let ping = bundle(
        ex.join("ping-pong/ping.toml"),
        ping_pong.clone(),
        "ping.rbp",
    )
    .await;
    let failing_manifest = dir.path().join("chaos-fail.toml");
    std::fs::write(
        &failing_manifest,
        "[app]\nname = \"chaos-fail\"\nversion = \"1.0.0\"\n\
         [appserver.lifecycle]\nstart_args = { fail = \"trap\" }\n\
         [appserver.supervision]\nintensity = 2\nbackoff = { initial_ms = 10, max_ms = 20 }\n\
         [appserver.capabilities.\"wasi:logging\"]\n",
    )
    .unwrap();
    let chaos_fail = bundle(failing_manifest, chaos, "chaos-fail.rbp").await;
    let line_server = bundle(
        ex.join("line-server/paddock.toml"),
        plain_wasm.join("example_line_server.wasm"),
        "line-server.rbp",
    )
    .await;
    let lines_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let toml = format!(
        "data_dir = {:?}\nadmin_socket = {:?}\nmax_instances = 16\ntrusted_keys = [{:?}, {:?}]\n[listeners.lines]\naddress = \"127.0.0.1:{lines_port}\"\n",
        dir.path().join("data"),
        socket,
        encrypted.stdout,
        plain.stdout
    );
    let node = Arc::new(
        Node::open(NodeConfig::from_toml_str(&toml).unwrap(), LogRing::new(100))
            .await
            .unwrap(),
    );
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
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // ping-pong: typed CBOR calls, the registry and a monitor.
    assert!(paddock(&socket, &["deploy", s(&pong)]).await.ok);
    assert!(paddock(&socket, &["deploy", s(&ping)]).await.ok);
    let first = paddock(&socket, &["call", "ping", "ping:41"]).await;
    let ps: Vec<paddock::api::InstanceInfo> =
        serde_json::from_str(&paddock(&socket, &["--json", "ps"]).await.stdout).unwrap();
    let pong_pid = ps.iter().find(|i| i.app == "pong").unwrap().pid.clone();
    assert!(paddock(&socket, &["kill", &pong_pid]).await.ok);
    let mut downs = String::new();
    for _ in 0..200 {
        downs = paddock(&socket, &["call", "ping", "downs"]).await.stdout;
        if downs == "1" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut again = String::new();
    for _ in 0..200 {
        again = paddock(&socket, &["call", "ping", "ping:1"]).await.stdout;
        if again == "pong 2" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(first.stdout, "pong 42");
    assert_eq!(downs, "1");
    assert_eq!(again, "pong 2");

    // counter: snapshot upgrade through #[derive(Snapshot)] migration.
    assert!(paddock(&socket, &["deploy", s(&c1)]).await.ok);
    for _ in 0..3 {
        paddock(&socket, &["call", "counter", "inc"]).await;
    }
    let up = paddock(&socket, &["upgrade", "counter", s(&c2)]).await;
    let after = paddock(&socket, &["call", "counter", "get"]).await;
    let broken = paddock(&socket, &["upgrade", "counter", s(&c3)]).await;
    let still = paddock(&socket, &["call", "counter", "get"]).await;
    assert!(
        up.stdout.starts_with("1.0.0 -> 2.0.0 (snapshot)"),
        "{} {}",
        up.stdout,
        up.stderr
    );
    assert_eq!(after.stdout, "2.0.0 count=3 history=");
    assert!(!broken.ok);
    assert!(
        broken.stderr.contains("v3 refuses snapshots from 2.0.0"),
        "{}",
        broken.stderr
    );
    assert_eq!(still.stdout, "2.0.0 count=3 history=");

    // line-server: a connection handler on a listener, handing `inc` to
    // the counter app by its registered name.
    let deployed = paddock(&socket, &["deploy", s(&line_server)]).await;
    assert!(deployed.ok, "{}", deployed.stderr);
    let (read, mut write) = tokio::net::TcpStream::connect(("127.0.0.1", lines_port))
        .await
        .unwrap()
        .into_split();
    let mut read = tokio::io::BufReader::new(read);
    let mut replies = Vec::new();
    for line in ["echo hello", "inc", "get"] {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let mut reply = String::new();
        read.read_line(&mut reply).await.unwrap();
        replies.push(reply.trim_end().to_string());
    }
    let status = paddock(&socket, &["status", "line-server"]).await;
    assert_eq!(replies, ["hello", "4", "2.0.0 count=4 history=4"]);
    assert!(status.stdout.contains("LISTENER"), "{}", status.stdout);

    // chaos: a crash loop in init is marked failed with the reason.
    let deployed = paddock(&socket, &["--json", "deploy", s(&chaos_fail)]).await;
    let status = paddock(&socket, &["status", "chaos-fail"]).await;
    assert!(deployed.ok, "{}", deployed.stderr);
    assert!(
        status.stdout.contains("status:    failed"),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("max restart intensity"),
        "{}",
        status.stdout
    );

    node.shutdown().await;
    let _ = stop.send(());
    server.await.unwrap();
}
