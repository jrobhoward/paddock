//! A node in a temporary directory, signed test bundles, and fixtures.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, OnceLock};

use minisign::KeyPair;
use paddock_api::Client;
use paddock_node::{LogRing, Node, NodeConfig, RingLayer};
use rustbridge_bundle::{BundleBuilder, Manifest, Platform};
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
    let (package, dir, once) = if name == "paddock_fixture_services" {
        (
            "paddock-fixture-services",
            "target/fixtures/services-plain",
            &SERVICES,
        )
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
    let dir = TempDir::new().unwrap();
    let wasm = dir.path().join(format!("{name}.wasm"));
    std::fs::write(&wasm, component).unwrap();
    let mut builder = BundleBuilder::new(Manifest::new(name, version))
        .add_component(Platform::Wasm32Wasip2, &wasm, WORLD)
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
                paddock_node::serve(node, async {
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
