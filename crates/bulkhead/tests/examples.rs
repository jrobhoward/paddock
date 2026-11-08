//! The SDK examples end to end (Linux): built, keyed and bundled with the
//! `bulkhead` binary, deployed to a node, and driven through the CLI.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bulkhead::node::{LogRing, Node, NodeConfig};
use tempfile::TempDir;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn bulkhead(socket: &Path, args: &[&str]) -> Run {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_bulkhead"))
        .env("BULKHEAD_KEY_PASSWORD", "test-password")
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
    if !cfg!(target_os = "linux") {
        eprintln!("not Linux: skipped");
        return;
    }
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("a.sock");
    let ex = root().join("examples");
    let built = common::examples(
        &[
            "example-counter",
            "example-ping-pong",
            "example-chaos",
            "example-line-server",
            "example-ledger",
        ],
        &[],
        "plain",
    );
    let counter = [
        built.of("example-counter"),
        common::examples(&["example-counter"], &["example-counter/v2"], "counter-v2")
            .of("example-counter"),
        common::examples(&["example-counter"], &["example-counter/v3"], "counter-v3")
            .of("example-counter"),
    ];
    let ping_pong = built.of("example-ping-pong");
    let chaos = built.of("example-chaos");

    // One encrypted key (opened with BULKHEAD_KEY_PASSWORD) for the first
    // bundle; a plain key for the rest, since key derivation is slow.
    let encrypted = bulkhead(&socket, &["keygen", s(&dir.path().join("signer"))]).await;
    let plain = bulkhead(
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
            let r = bulkhead(
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
        ex.join("counter/bulkhead.toml"),
        counter[0].clone(),
        "c1.rbp",
    )
    .await;
    let c2 = bundle(
        ex.join("counter/bulkhead-v2.toml"),
        counter[1].clone(),
        "c2.rbp",
    )
    .await;
    let c3 = bundle(
        ex.join("counter/bulkhead-v3.toml"),
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
        ex.join("line-server/bulkhead.toml"),
        built.of("example-line-server"),
        "line-server.rbp",
    )
    .await;
    let ledger = bundle(
        ex.join("ledger/bulkhead.toml"),
        built.of("example-ledger"),
        "ledger.rbp",
    )
    .await;
    let lines_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let policy = dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.ledger]\ngrant = [\"wasi:keyvalue\"]\nbuckets = [\"ledger\"]\n",
    )
    .unwrap();
    let toml = format!(
        "data_dir = {:?}\nadmin_socket = {:?}\nmax_instances = 16\ntrusted_keys = [{:?}, {:?}]\npolicy = {policy:?}\n[listeners.lines]\naddress = \"127.0.0.1:{lines_port}\"\n\
         [buckets.ledger]\ndurability = \"disk\"\n{}",
        dir.path().join("data"),
        socket,
        encrypted.stdout,
        plain.stdout,
        common::runner_config()
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
            bulkhead::node::serve(node, async {
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
    assert!(bulkhead(&socket, &["deploy", s(&pong)]).await.ok);
    assert!(bulkhead(&socket, &["deploy", s(&ping)]).await.ok);
    let first = bulkhead(&socket, &["call", "ping", "ping:41"]).await;
    let ps: Vec<bulkhead::api::InstanceInfo> =
        serde_json::from_str(&bulkhead(&socket, &["--json", "ps"]).await.stdout).unwrap();
    let pong_pid = ps.iter().find(|i| i.app == "pong").unwrap().pid.clone();
    assert!(bulkhead(&socket, &["kill", &pong_pid]).await.ok);
    let mut downs = String::new();
    for _ in 0..200 {
        downs = bulkhead(&socket, &["call", "ping", "downs"]).await.stdout;
        if downs == "1" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut again = String::new();
    for _ in 0..200 {
        again = bulkhead(&socket, &["call", "ping", "ping:1"]).await.stdout;
        if again == "pong 2" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(first.stdout, "pong 42");
    assert_eq!(downs, "1");
    assert_eq!(again, "pong 2");

    // counter: snapshot upgrade through #[derive(Snapshot)] migration.
    assert!(bulkhead(&socket, &["deploy", s(&c1)]).await.ok);
    for _ in 0..3 {
        bulkhead(&socket, &["call", "counter", "inc"]).await;
    }
    let up = bulkhead(&socket, &["upgrade", "counter", s(&c2)]).await;
    let after = bulkhead(&socket, &["call", "counter", "get"]).await;
    let broken = bulkhead(&socket, &["upgrade", "counter", s(&c3)]).await;
    let still = bulkhead(&socket, &["call", "counter", "get"]).await;
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
    let deployed = bulkhead(&socket, &["deploy", s(&line_server)]).await;
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
    let status = bulkhead(&socket, &["status", "line-server"]).await;
    assert_eq!(replies, ["hello", "4", "2.0.0 count=4 history=4"]);
    assert!(status.stdout.contains("LISTENER"), "{}", status.stdout);

    // chaos: a crash loop in init is marked failed with the reason.
    let deployed = bulkhead(&socket, &["--json", "deploy", s(&chaos_fail)]).await;
    let status = bulkhead(&socket, &["status", "chaos-fail"]).await;
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

    // ledger: entries in a shared bucket (node-scoped here; on a pair,
    // `scope = "cluster"` replicates it, as the replication soak runs it).
    let deployed = bulkhead(&socket, &["deploy", s(&ledger)]).await;
    assert!(deployed.ok, "{}", deployed.stderr);
    let mut ledger_replies = Vec::new();
    for command in ["put a 1", "put b 2", "get a", "latest b", "get zz", "keys"] {
        ledger_replies.push(bulkhead(&socket, &["call", "ledger", command]).await.stdout);
    }
    assert_eq!(ledger_replies, ["ok", "ok", "1", "2", "none", "a,b"]);

    node.shutdown().await;
    let _ = stop.send(());
    server.await.unwrap();
}
