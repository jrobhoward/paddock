//! A node in a temporary directory, signed test bundles, and fixtures.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use bulkhead::api::{Client, ClientError};
use bulkhead::node::{LogRing, Node, NodeConfig, RingLayer};
use minisign::KeyPair;
use rustbridge_bundle::{BundleBuilder, Manifest, Platform};
use serde_json::json;
use tempfile::TempDir;
use tracing_subscriber::layer::SubscriberExt;

pub const WORLD: &str = "bulkhead:runtime/app@0.1.0";

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

/// A fixture component (`bulkhead_fixture_chaos`, `..._services`). The
/// services fixture is built without its `denied` feature, into its own
/// directory, so it imports only interfaces a node provides.
pub fn fixture(name: &str) -> Vec<u8> {
    static CHAOS: OnceLock<()> = OnceLock::new();
    static SERVICES: OnceLock<()> = OnceLock::new();
    static SDK: OnceLock<()> = OnceLock::new();
    static NET: OnceLock<()> = OnceLock::new();
    let (package, dir, once) = if name == "bulkhead_fixture_services" {
        (
            "bulkhead-fixture-services",
            "target/fixtures/services-plain",
            &SERVICES,
        )
    } else if name == "bulkhead_fixture_sdk" {
        ("bulkhead-fixture-sdk", "target/fixtures/sdk-plain", &SDK)
    } else if name == "bulkhead_fixture_net" {
        ("bulkhead-fixture-net", "target/fixtures/net-plain", &NET)
    } else {
        (
            "bulkhead-fixture-chaos",
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
        build("bulkhead-fixture-counter", feature.as_deref(), &target);
    });
    std::fs::read(target.join("wasm32-wasip2/release/bulkhead_fixture_counter.wasm")).unwrap()
}

/// The connection fixture (`bulkhead-fixture-conn`), v1 or v2 (`echo`
/// replies `v2:<text>`).
pub fn conn(version: u8) -> Vec<u8> {
    static BUILT: [OnceLock<()>; 2] = [OnceLock::new(), OnceLock::new()];
    let target = workspace_root().join(format!("target/fixtures/conn-v{version}"));
    BUILT[usize::from(version - 1)].get_or_init(|| {
        let feature = (version > 1).then_some("v2");
        build("bulkhead-fixture-conn", feature, &target);
    });
    std::fs::read(target.join("wasm32-wasip2/release/bulkhead_fixture_conn.wasm")).unwrap()
}

/// A signed connection-handler bundle (`bulkhead:connection` world).
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
        bulkhead_bundle_world(),
    )
}

/// The HTTP-handler fixture (`bulkhead-fixture-http`): version 2 answers
/// `/hello` with `hello v2`.
pub fn http_handler(version: u8) -> Vec<u8> {
    static BUILT: [OnceLock<()>; 2] = [OnceLock::new(), OnceLock::new()];
    let target = workspace_root().join(format!("target/fixtures/http-handler-v{version}"));
    BUILT[usize::from(version - 1)].get_or_init(|| {
        let feature = (version > 1).then_some("v2");
        build("bulkhead-fixture-http", feature, &target);
    });
    std::fs::read(target.join("wasm32-wasip2/release/bulkhead_fixture_http.wasm")).unwrap()
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
        bulkhead_bundle::HTTP_HANDLER_WORLD,
    )
}

fn bulkhead_bundle_world() -> &'static str {
    "bulkhead:connection/connection-handler@0.1.0"
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

/// `bulkhead-runner` and the native fixture executable, built natively on
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
                "bulkhead-native",
                "-p",
                "bulkhead-fixture-native",
                "--target-dir",
            ])
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success(), "building native fixtures failed");
        let debug = target.join("debug");
        (
            debug.join("bulkhead-runner"),
            debug.join("bulkhead-fixture-native"),
        )
    })
}

/// A fixture crate's program, built natively with `features` on first use
/// by each test binary, into `target/fixtures/<dir>`.
fn native_program(package: &str, features: &str, dir: &str, once: &OnceLock<()>) -> Vec<u8> {
    let target = workspace_root().join("target/fixtures").join(dir);
    once.get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(workspace_root())
            .args(["build", "-p", package, "--features", features, "--bin"])
            .arg(package)
            .arg("--target-dir")
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success(), "building {package} natively failed");
    });
    std::fs::read(target.join("debug").join(package)).unwrap()
}

/// The native fixture executable (`bulkhead-fixture-native`).
pub fn native_app() -> Vec<u8> {
    std::fs::read(&native_fixtures().1).unwrap()
}

/// The counter fixture at `version` (1, 2 or 3) as a native program.
pub fn native_counter(version: u8) -> Vec<u8> {
    static BUILT: [OnceLock<()>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    let features = if version > 1 {
        format!("native,v{version}")
    } else {
        "native".to_string()
    };
    native_program(
        "bulkhead-fixture-counter",
        &features,
        &format!("native-counter-v{version}"),
        &BUILT[usize::from(version - 1)],
    )
}

/// The SDK fixture as a native program.
pub fn native_sdk() -> Vec<u8> {
    static BUILT: OnceLock<()> = OnceLock::new();
    native_program("bulkhead-fixture-sdk", "native", "native-sdk", &BUILT)
}

/// The conformance app as a native program: the chaos fixture's commands
/// and pid format, on the SDK.
pub fn native_conformance() -> Vec<u8> {
    static BUILT: OnceLock<()> = OnceLock::new();
    native_program(
        "bulkhead-fixture-conformance",
        "native",
        "native-conformance",
        &BUILT,
    )
}

/// The net fixture as a native program.
pub fn native_net() -> Vec<u8> {
    static BUILT: OnceLock<()> = OnceLock::new();
    native_program("bulkhead-fixture-net", "native", "native-net", &BUILT)
}

/// A signed `.rbp` running `program` as a native-process app under
/// `appserver`, which need not say so.
pub fn app_bundle(
    key: &KeyPair,
    name: &str,
    version: &str,
    program: &[u8],
    appserver: serde_json::Value,
) -> Vec<u8> {
    app_bundle_with(key, name, version, program, appserver, &[])
}

/// [`app_bundle`] with checksummed `files` (`priv/...`).
pub fn app_bundle_with(
    key: &KeyPair,
    name: &str,
    version: &str,
    program: &[u8],
    mut appserver: serde_json::Value,
    files: &[(&str, &[u8])],
) -> Vec<u8> {
    appserver["kind"] = json!("native-process");
    appserver["native"] = json!({ "program": "executable" });
    let platform = Platform::current().unwrap();
    native_bundle_files(key, name, version, &[(platform, program)], appserver, files)
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
    native_bundle_for(
        key,
        name,
        version,
        &[(Platform::current().unwrap(), program)],
        appserver,
    )
}

/// The other Linux platform than this machine's, and the start of an
/// ELF file for it: enough to bundle, not to run.
pub fn other_platform_elf() -> (Platform, Vec<u8>) {
    let (platform, machine) = if Platform::current() == Some(Platform::LinuxAarch64) {
        (Platform::LinuxX86_64, 62u16)
    } else {
        (Platform::LinuxAarch64, 183u16)
    };
    let mut header = vec![0u8; 64];
    header[..4].copy_from_slice(b"\x7fELF");
    header[4] = 2;
    header[5] = 1;
    header[18..20].copy_from_slice(&machine.to_le_bytes());
    (platform, header)
}

/// A signed `.rbp` of a native-process app with a program per platform.
pub fn native_bundle_for(
    key: &KeyPair,
    name: &str,
    version: &str,
    programs: &[(Platform, &[u8])],
    appserver: serde_json::Value,
) -> Vec<u8> {
    native_bundle_files(key, name, version, programs, appserver, &[])
}

fn native_bundle_files(
    key: &KeyPair,
    name: &str,
    version: &str,
    programs: &[(Platform, &[u8])],
    appserver: serde_json::Value,
    files: &[(&str, &[u8])],
) -> Vec<u8> {
    let dir = TempDir::new().unwrap();
    let mut builder = BundleBuilder::new(Manifest::new(name, version));
    for (path, bytes) in files {
        builder = builder.add_checksummed_bytes(path, bytes.to_vec());
    }
    for (platform, program) in programs {
        let lib = dir.path().join(platform.as_str()).join(name);
        std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
        std::fs::write(&lib, program).unwrap();
        builder = builder.add_library(*platform, &lib).unwrap();
    }
    let out = dir.path().join("out.rbp");
    builder
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
    /// [`TestNode::start`] with the test-built `bulkhead-runner`, for
    /// native apps.
    pub async fn start_native(extra: &str) -> Self {
        Self::start(&format!(
            "{extra}\n[native]\nrunner = {:?}\n",
            native_fixtures().0
        ))
        .await
    }

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
                bulkhead::node::serve(node, async {
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

/// Wait for a `bulkhead node` process to create its admin socket. Panics
/// with the node's log if the process exits first, or if the socket is not
/// there within 30 s (a node starts in well under a second, but a machine
/// running the whole test suite can stall it for several).
pub async fn wait_for_socket(child: &mut tokio::process::Child, socket: &Path, log: &Path) {
    for _ in 0..1200 {
        if socket.exists() {
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "node exited ({status}) before creating {}; its log:\n{}",
                socket.display(),
                std::fs::read_to_string(log).unwrap_or_default()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!(
        "no admin socket at {} after 30 s; the node's log:\n{}",
        socket.display(),
        std::fs::read_to_string(log).unwrap_or_default()
    );
}

/// Which build of the examples (`tests/examples.rs`, `tests/scada.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExampleBuild {
    /// For `wasm32-wasip2`.
    Component,
    /// A native program (feature `native`), Linux only.
    Native,
}

/// Where one build of some examples landed.
pub struct Built {
    dir: PathBuf,
    build: ExampleBuild,
}

impl Built {
    /// The component or program of the package `package`.
    pub fn of(&self, package: &str) -> PathBuf {
        match self.build {
            ExampleBuild::Component => self.dir.join(format!("{}.wasm", package.replace('-', "_"))),
            ExampleBuild::Native => self.dir.join(package),
        }
    }
}

impl ExampleBuild {
    /// Build `packages` together into `target/fixtures/examples-<tag>`
    /// (one directory per feature set, since features unify within a
    /// build; the native build's in its own), with `features` (`package/name`).
    pub fn packages(self, packages: &[&str], features: &[&str], tag: &str) -> Built {
        let native = self == ExampleBuild::Native;
        let target = workspace_root().join(format!(
            "target/fixtures/examples-{tag}{}",
            if native { "-native" } else { "" }
        ));
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.current_dir(workspace_root())
            .args(["build", "--release"]);
        if !native {
            cmd.args(["--target", "wasm32-wasip2"]);
        }
        for p in packages {
            cmd.args(["-p", p]);
        }
        cmd.arg("--target-dir").arg(&target);
        let mut features: Vec<String> = features.iter().map(ToString::to_string).collect();
        if native {
            features.extend(packages.iter().map(|p| format!("{p}/native")));
        }
        if !features.is_empty() {
            cmd.args(["--features", &features.join(",")]);
        }
        assert!(
            cmd.status().unwrap().success(),
            "building {packages:?} ({self:?}) failed"
        );
        Built {
            dir: if native {
                target.join("release")
            } else {
                target.join("wasm32-wasip2/release")
            },
            build: self,
        }
    }

    /// The manifest at `path`; for the native build, a copy in `into`
    /// (named after `out`) that runs the program as a native process.
    pub fn manifest(self, path: &Path, into: &Path, out: &str) -> PathBuf {
        if self == ExampleBuild::Component {
            return path.to_path_buf();
        }
        let mut doc: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
        let appserver = doc
            .entry("appserver")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .unwrap();
        appserver.insert("kind".into(), "native-process".into());
        appserver.insert(
            "native".into(),
            toml::Value::Table(toml::Table::from_iter([(
                "program".to_string(),
                toml::Value::from("executable"),
            )])),
        );
        let native = into.join(format!("native-{out}.toml"));
        std::fs::write(&native, toml::to_string(&doc).unwrap()).unwrap();
        native
    }

    /// Node config this build needs: the native build's runner.
    pub fn node_extra(self) -> String {
        match self {
            ExampleBuild::Native => format!("[native]\nrunner = {:?}\n", native_fixtures().0),
            ExampleBuild::Component => String::new(),
        }
    }
}
