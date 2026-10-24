//! HTTP ingress through a node: `examples/http-counter`
//! bundled with the CLI, deployed under operator policy, and driven over
//! HTTP; route clashes, ungranted routes, stop and start.

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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn build() -> PathBuf {
    let target = root().join("target/fixtures/examples-http");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = StdCommand::new(cargo)
        .current_dir(root())
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .args(["-p", "example-http-counter", "--target-dir"])
        .arg(&target)
        .status()
        .unwrap()
        .success();
    assert!(ok, "building example-http-counter failed");
    target.join("wasm32-wasip2/release/example_http_counter.wasm")
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn paddock(socket: &Path, args: &[&str]) -> Run {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_paddock"))
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One request on its own connection: (status, body).
async fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (
        head.split(' ').nth(1).unwrap().parse().unwrap(),
        body.to_string(),
    )
}

struct Setup {
    dir: TempDir,
    key: PathBuf,
    public: String,
    wasm: PathBuf,
}

async fn setup() -> Setup {
    let wasm = build();
    let dir = TempDir::new().unwrap();
    let keys = paddock(
        &dir.path().join("x.sock"),
        &["keygen", "--no-password", s(&dir.path().join("k"))],
    )
    .await;
    assert!(keys.ok, "{}", keys.stderr);
    Setup {
        key: dir.path().join("k.key"),
        public: keys.stdout,
        dir,
        wasm,
    }
}

/// Bundle the counter's component as `name` with `routes` (TOML).
async fn bundle(setup: &Setup, name: &str, routes: &str) -> PathBuf {
    let manifest = setup.dir.path().join(format!("{name}.toml"));
    std::fs::write(
        &manifest,
        format!(
            "[app]\nname = \"{name}\"\nversion = \"1.0.0\"\n\
             [appserver.capabilities.\"wasi:http/incoming-handler\"]\nroutes = {routes}\n"
        ),
    )
    .unwrap();
    let out = setup.dir.path().join(format!("{name}.rbp"));
    let r = paddock(
        &setup.dir.path().join("x.sock"),
        &[
            "bundle",
            s(&manifest),
            s(&setup.wasm),
            "--key",
            s(&setup.key),
            "-o",
            s(&out),
        ],
    )
    .await;
    assert!(r.ok, "{}", r.stderr);
    out
}

async fn node(setup: &Setup, tag: &str, extra: &str) -> (Arc<Node>, PathBuf) {
    let socket = setup.dir.path().join(format!("{tag}.sock"));
    let policy = setup.dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.http-counter]\ngrant = [\"wasi:http/incoming-handler\"]\n\
         [apps.thief]\ngrant = [\"wasi:http/incoming-handler\"]\n",
    )
    .unwrap();
    let toml = format!(
        "data_dir = {:?}\nadmin_socket = {socket:?}\ntrusted_keys = [{:?}]\npolicy = {policy:?}\n{extra}",
        setup.dir.path().join(tag),
        setup.public,
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
    (node, socket)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_counter____deployed_with_policy____served_and_routes_follow_the_app() {
    let setup = setup().await;
    let port = free_port();
    let (node, socket) = node(
        &setup,
        "a",
        &format!("[http.main]\naddress = \"127.0.0.1:{port}\"\n"),
    )
    .await;
    let counter = bundle(&setup, "http-counter", "[{ path_prefix = \"/counter\" }]").await;
    let thief = bundle(&setup, "thief", "[{ path_prefix = \"/counter\" }]").await;
    let ungranted = bundle(&setup, "ungranted", "[{ path_prefix = \"/free\" }]").await;

    let deployed = paddock(&socket, &["deploy", s(&counter)]).await;
    let first = http(port, "GET", "/counter", "").await;
    let posted = http(port, "POST", "/counter", "5").await;
    let wrong = http(port, "PUT", "/counter", "").await;
    let clash = paddock(&socket, &["deploy", s(&thief)]).await;
    let free = paddock(&socket, &["--json", "deploy", s(&ungranted)]).await;
    let not_routed = http(port, "GET", "/free", "").await;
    assert!(paddock(&socket, &["stop", "http-counter"]).await.ok);
    let stopped = http(port, "GET", "/counter", "").await;
    assert!(paddock(&socket, &["start", "http-counter"]).await.ok);
    let started = http(port, "GET", "/counter", "").await;

    assert!(deployed.ok, "{}", deployed.stderr);
    assert_eq!(first, (200, r#"{"count":0}"#.to_string()));
    assert_eq!(posted, (200, r#"{"count":5}"#.to_string()));
    assert_eq!(wrong.0, 405);
    assert!(!clash.ok);
    assert!(
        clash.stderr.contains("belongs to http-counter"),
        "{}",
        clash.stderr
    );
    assert!(free.ok, "{}", free.stderr);
    assert!(
        free.stdout.contains("wasi:http/incoming-handler"),
        "{}",
        free.stdout
    );
    assert_eq!(not_routed.0, 404);
    assert_eq!(stopped.0, 404);
    assert_eq!(started, (200, r#"{"count":0}"#.to_string()));
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routes____node_without_ingress____deploy_refused() {
    let setup = setup().await;
    let (node, socket) = node(&setup, "b", "").await;
    let counter = bundle(&setup, "http-counter", "[{ path_prefix = \"/counter\" }]").await;

    let refused = paddock(&socket, &["deploy", s(&counter)]).await;

    assert!(!refused.ok);
    assert!(
        refused.stderr.contains("no HTTP ingress"),
        "{}",
        refused.stderr
    );
    node.shutdown().await;
}

/// A self-signed certificate for `host` written to `cert` and `key`; its
/// DER.
fn write_cert(cert: &Path, key: &Path, host: &str) -> Vec<u8> {
    let made = rcgen::generate_simple_self_signed(vec![host.to_string()]).unwrap();
    std::fs::write(cert, made.cert.pem()).unwrap();
    std::fs::write(key, made.signing_key.serialize_pem()).unwrap();
    made.cert.der().to_vec()
}

/// One HTTPS GET trusting `trusted`: (the certificate served, status).
async fn https_get(port: u16, trusted: &[&Vec<u8>], path: &str) -> (Vec<u8>, u16) {
    let mut roots = rustls::RootCertStore::empty();
    for der in trusted {
        roots
            .add(rustls_pki_types::CertificateDer::from((*der).clone()))
            .unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let name = rustls_pki_types::ServerName::try_from("shop.example").unwrap();
    let mut stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .unwrap();
    let served = stream.get_ref().1.peer_certificates().unwrap()[0].to_vec();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nhost: shop.example\r\nconnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw).await;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    (served, raw.split(' ').nth(1).unwrap().parse().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn https____reload_tls____new_certificate_and_a_broken_one_refused() {
    let setup = setup().await;
    let port = free_port();
    let cert = setup.dir.path().join("shop.pem");
    let key = setup.dir.path().join("shop.key");
    let first = write_cert(&cert, &key, "shop.example");
    let (node, socket) = node(
        &setup,
        "tls",
        &format!(
            "[http.main]\naddress = \"127.0.0.1:{port}\"\ntls_reload_s = 0\n\
             tls = [{{ cert = {cert:?}, key = {key:?}, hosts = [\"shop.example\"] }}]\n"
        ),
    )
    .await;
    let counter = bundle(&setup, "http-counter", "[{ path_prefix = \"/counter\" }]").await;
    assert!(paddock(&socket, &["deploy", s(&counter)]).await.ok);

    let (served_first, status) = https_get(port, &[&first], "/counter").await;
    let second = write_cert(&cert, &key, "shop.example");
    let reloaded = paddock(&socket, &["reload-tls"]).await;
    let (served_second, _) = https_get(port, &[&first, &second], "/counter").await;
    std::fs::write(&cert, "broken").unwrap();
    let refused = paddock(&socket, &["reload-tls"]).await;
    let (still, _) = https_get(port, &[&first, &second], "/counter").await;

    assert_eq!((served_first, status), (first.clone(), 200));
    assert!(reloaded.ok, "{}", reloaded.stderr);
    assert_eq!(served_second, second);
    assert!(!refused.ok);
    assert!(refused.stderr.contains("http.main"), "{}", refused.stderr);
    assert_eq!(still, second);
    node.shutdown().await;
}
