//! A node in a temporary directory, signed test bundles, and fixtures.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, OnceLock};

use minisign::KeyPair;
use paddock::api::{Client, ClientError};
use paddock::node::{LogRing, Node, NodeConfig, RingLayer};
use rustbridge_bundle::{BundleBuilder, Manifest, Platform};
use serde_json::json;
use tempfile::TempDir;
use tracing_subscriber::layer::SubscriberExt;

pub const WORLD: &str = "paddock:runtime/app@0.1.0";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn build(package: &str, features: Option<&str>, target_dir: &PathBuf) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(workspace_root())
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip2",
            "-p",
            package,
            "--target-dir",
        ])
        .arg(target_dir);
    if let Some(f) = features {
        cmd.args(["--features", f]);
    }
    assert!(cmd.status().unwrap().success(), "building {package} failed");
}

/// A fixture component (`paddock_fixture_chaos`, `..._services`). The
/// services fixture is built without its `denied` feature, into its own
/// directory, so it imports only interfaces a node provides.
pub fn fixture(name: &str) -> Vec<u8> {
    static CHAOS: OnceLock<()> = OnceLock::new();
    static SERVICES: OnceLock<()> = OnceLock::new();
    static SDK: OnceLock<()> = OnceLock::new();
    static NET: OnceLock<()> = OnceLock::new();
    let (package, dir, once) = if name == "paddock_fixture_services" {
        (
            "paddock-fixture-services",
            "target/fixtures/services-plain",
            &SERVICES,
        )
    } else if name == "paddock_fixture_sdk" {
        ("paddock-fixture-sdk", "target/fixtures/sdk-plain", &SDK)
    } else if name == "paddock_fixture_net" {
        ("paddock-fixture-net", "target/fixtures/net-plain", &NET)
    } else {
        (
            "paddock-fixture-chaos",
            "target/fixtures/chaos-plain",
            &CHAOS,
        )
    };
    let target = workspace_root().join(dir);
    once.get_or_init(|| build(package, None, &target));
    std::fs::read(target.join(format!("wasm32-wasip2/release/{name}.wasm"))).unwrap()
}

/// The counter fixture at `version` (1, 2 or 3).
pub fn counter(version: u8) -> Vec<u8> {
    static BUILT: [OnceLock<()>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    let target = workspace_root().join(format!("target/fixtures/counter-v{version}"));
    BUILT[usize::from(version - 1)].get_or_init(|| {
        let feature = (version > 1).then(|| format!("v{version}"));
        build("paddock-fixture-counter", feature.as_deref(), &target);
    });
    std::fs::read(target.join("wasm32-wasip2/release/paddock_fixture_counter.wasm")).unwrap()
}

/// The connection fixture (`paddock-fixture-conn`), v1 or v2 (`echo`
/// replies `v2:<text>`).
pub fn conn(version: u8) -> Vec<u8> {
    static BUILT: [OnceLock<()>; 2] = [OnceLock::new(), OnceLock::new()];
    let target = workspace_root().join(format!("target/fixtures/conn-v{version}"));
    BUILT[usize::from(version - 1)].get_or_init(|| {
        let feature = (version > 1).then_some("v2");
        build("paddock-fixture-conn", feature, &target);
    });
    std::fs::read(target.join("wasm32-wasip2/release/paddock_fixture_conn.wasm")).unwrap()
}

/// A signed connection-handler bundle (`paddock:connection` world).
pub fn conn_bundle(
    key: &KeyPair,
    name: &str,
    version: &str,
    component: &[u8],
    appserver: serde_json::Value,
) -> Vec<u8> {
    bundle_world(
        key,
        name,
        version,
        component,
        appserver,
        &[],
        paddock_bundle_world(),
    )
}

/// The HTTP-handler fixture (`paddock-fixture-http`): version 2 answers
/// `/hello` with `hello v2`.
pub fn http_handler(version: u8) -> Vec<u8> {
    static BUILT: [OnceLock<()>; 2] = [OnceLock::new(), OnceLock::new()];
    let target = workspace_root().join(format!("target/fixtures/http-handler-v{version}"));
    BUILT[usize::from(version - 1)].get_or_init(|| {
        let feature = (version > 1).then_some("v2");
        build("paddock-fixture-http", feature, &target);
    });
    std::fs::read(target.join("wasm32-wasip2/release/paddock_fixture_http.wasm")).unwrap()
}

/// A signed HTTP-handler bundle (`wasi:http/proxy` world).
pub fn http_handler_bundle(
    key: &KeyPair,
    name: &str,
    version: &str,
    component: &[u8],
    appserver: serde_json::Value,
) -> Vec<u8> {
    bundle_world(
        key,
        name,
        version,
        component,
        appserver,
        &[],
        paddock_bundle::HTTP_HANDLER_WORLD,
    )
}

fn paddock_bundle_world() -> &'static str {
    "paddock:connection/connection-handler@0.1.0"
}

/// A signed `.rbp` with `component` and `appserver`, plus checksummed
/// files.
pub fn bundle(
    key: &KeyPair,
    name: &str,
    version: &str,
    component: &[u8],
    appserver: serde_json::Value,
    files: &[(&str, &[u8])],
) -> Vec<u8> {
    bundle_world(key, name, version, component, appserver, files, WORLD)
}

fn bundle_world(
    key: &KeyPair,
    name: &str,
    version: &str,
    component: &[u8],
    appserver: serde_json::Value,
    files: &[(&str, &[u8])],
    world: &str,
) -> Vec<u8> {
    let dir = TempDir::new().unwrap();
    let wasm = dir.path().join(format!("{name}.wasm"));
    std::fs::write(&wasm, component).unwrap();
    let mut builder = BundleBuilder::new(Manifest::new(name, version))
        .add_component(Platform::Wasm32Wasip2, &wasm, world)
        .unwrap()
        .with_extension("appserver", appserver)
        .with_signing_key(key.pk.to_base64(), key.sk.clone());
    for (path, bytes) in files {
        builder = builder.add_checksummed_bytes(path, bytes.to_vec());
    }
    let out = dir.path().join("out.rbp");
    builder.write(&out).unwrap();
    std::fs::read(out).unwrap()
}

/// `paddock-runner` and the native fixture executable, built natively on
/// first use into their own target directory.
pub fn native_fixtures() -> &'static (PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let target = workspace_root().join("target/fixtures/native");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(workspace_root())
            .args([
                "build",
                "-p",
                "paddock-native",
                "-p",
                "paddock-fixture-native",
                "--target-dir",
            ])
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success(), "building native fixtures failed");
        let debug = target.join("debug");
        (
            debug.join("paddock-runner"),
            debug.join("paddock-fixture-native"),
        )
    })
}

/// A signed `.rbp` of a native-process app: `program` as the release
/// variant for this machine's platform, and `appserver`.
pub fn native_bundle(
    key: &KeyPair,
    name: &str,
    version: &str,
    program: &[u8],
    appserver: serde_json::Value,
) -> Vec<u8> {
    let dir = TempDir::new().unwrap();
    let lib = dir.path().join(name);
    std::fs::write(&lib, program).unwrap();
    let out = dir.path().join("out.rbp");
    BundleBuilder::new(Manifest::new(name, version))
        .add_library(Platform::current().unwrap(), &lib)
        .unwrap()
        .with_extension("appserver", appserver)
        .with_signing_key(key.pk.to_base64(), key.sk.clone())
        .write(&out)
        .unwrap();
    std::fs::read(out).unwrap()
}

/// The log ring every test node shares (one global subscriber).
pub fn ring() -> Arc<LogRing> {
    static RING: OnceLock<Arc<LogRing>> = OnceLock::new();
    RING.get_or_init(|| {
        let ring = LogRing::new(10_000);
        let subscriber = tracing_subscriber::registry().with(RingLayer::new(ring.clone()));
        let _ = tracing::subscriber::set_global_default(subscriber);
        ring
    })
    .clone()
}

/// A running node with its admin API.
pub struct TestNode {
    pub dir: TempDir,
    pub key: KeyPair,
    node: Option<Arc<Node>>,
    pub client: Client,
    extra: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl TestNode {
    /// A node trusting a fresh key, with `extra` TOML appended to its
    /// config (policy path, volumes, config overrides).
    pub async fn start(extra: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let key = KeyPair::generate_unencrypted_keypair().unwrap();
        let (node, client, stop, server) = Self::boot(&dir, &key, extra).await;
        Self {
            dir,
            key,
            node: Some(node),
            client,
            extra: extra.to_string(),
            stop: Some(stop),
            server: Some(server),
        }
    }

    async fn boot(
        dir: &TempDir,
        key: &KeyPair,
        extra: &str,
    ) -> (
        Arc<Node>,
        Client,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let toml = format!(
            "data_dir = {:?}\nmax_instances = 32\ntrusted_keys = [{:?}]\n{extra}",
            dir.path().join("data"),
            key.pk.to_base64()
        );
        // Opening fails before restoring anything while a previous node's
        // runtime still holds the keyvalue database; retry until it is free.
        let mut tries = 0;
        let node = loop {
            match Node::open(NodeConfig::from_toml_str(&toml).unwrap(), ring()).await {
                Ok(node) => break Arc::new(node),
                Err(e) if tries < 250 => {
                    tries += 1;
                    let _ = e;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => panic!("{e}"),
            }
        };
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
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        (node, Client::new(&socket), stop, server)
    }

    pub fn node(&self) -> &Arc<Node> {
        self.node.as_ref().unwrap()
    }

    pub fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    pub fn audit(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.data().join("audit.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub fn events(&self) -> Vec<String> {
        self.audit()
            .iter()
            .map(|r| r["event"].as_str().unwrap().to_string())
            .collect()
    }

    /// Stop every app and the API, then start a new node on the same data.
    pub async fn restart(&mut self) {
        self.shutdown().await;
        // Release the old runtime (and its keyvalue database) first.
        self.node = None;
        let (node, client, stop, server) = Self::boot(&self.dir, &self.key, &self.extra).await;
        self.node = Some(node);
        self.client = client;
        self.stop = Some(stop);
        self.server = Some(server);
    }

    /// Drop the node, after `shutdown`, so its data directory is free.
    pub fn release(&mut self) {
        self.node = None;
    }

    pub async fn shutdown(&mut self) {
        if let Some(node) = &self.node {
            node.shutdown().await;
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            let _ = server.await;
        }
    }
}

/// `extra` with fast supervision backoff and short lifecycle timeouts.
pub fn fast(mut extra: serde_json::Value) -> serde_json::Value {
    let base = json!({
        "supervision": { "intensity": 3, "backoff": { "initial_ms": 10, "max_ms": 20 } },
        "lifecycle": { "init_timeout_ms": 2000, "shutdown_timeout_ms": 300 }
    });
    if let (Some(e), serde_json::Value::Object(b)) = (extra.as_object_mut(), base) {
        for (k, v) in b {
            e.entry(k).or_insert(v);
        }
    }
    extra
}

/// An API error's kind, or the error text for transport failures.
pub fn kind(err: &ClientError) -> String {
    match err {
        ClientError::Api(e) => e.kind.clone(),
        other => format!("{other}"),
    }
}

/// Call `app` with a text body and return the text reply.
pub async fn call(node: &TestNode, app: &str, body: &str) -> String {
    let (_, reply) = node
        .client
        .call(app, "text/plain", body.as_bytes().to_vec(), 5_000)
        .await
        .unwrap();
    String::from_utf8(reply).unwrap()
}

/// The services fixture's manifest: logging, config, a disk bucket and
/// a `/data` volume named `scratch`.
pub fn services_manifest() -> serde_json::Value {
    fast(json!({
        "capabilities": {
            "wasi:logging": {},
            "wasi:config": {},
            "wasi:keyvalue": { "buckets": { "data": { "durability": "disk" } } },
            "wasi:filesystem": { "preopens": { "/data": { "volume": "scratch", "mode": "rw" } } }
        },
        "config": { "defaults": { "greeting": "hello", "mode": "default" } }
    }))
}

/// The counter fixture's manifest, snapshot upgrades from `from`.
pub fn counter_manifest(from: Option<&str>) -> serde_json::Value {
    let mut m = fast(json!({}));
    if let Some(f) = from {
        m["upgrade"] = json!({ "mode": "snapshot", "snapshot_version": 1, "from_versions": [f] });
    } else {
        m["upgrade"] = json!({ "mode": "snapshot", "snapshot_version": 1 });
    }
    m
}
