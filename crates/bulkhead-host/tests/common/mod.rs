//! Builds guest fixtures and sets up runtimes for tests.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

pub mod pg;

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use bulkhead_bundle::manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};
use bulkhead_host::{Envelope, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The fixture component `name` (`bulkhead_fixture_chaos`), built for
/// `wasm32-wasip2` on first use. Builds into `target/fixtures`, because the
/// `cargo test` that runs this holds the lock on the main target directory.
pub fn fixture(name: &str) -> Vec<u8> {
    static BUILT: OnceLock<()> = OnceLock::new();
    let root = workspace_root();
    let target_dir = root.join("target/fixtures");
    BUILT.get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "-p",
                "bulkhead-fixture-chaos",
                "-p",
                "bulkhead-fixture-echo",
                "-p",
                "bulkhead-fixture-sdk",
                "-p",
                "bulkhead-fixture-conn",
                "-p",
                "bulkhead-fixture-conformance",
                "-p",
                "bulkhead-fixture-http",
                "-p",
                "bulkhead-fixture-net",
                "-p",
                "bulkhead-fixture-services",
                "--features",
                "bulkhead-fixture-services/denied",
                "--target-dir",
            ])
            .arg(&target_dir)
            .status()
            .expect("run cargo to build guest fixtures");
        assert!(
            status.success(),
            "building guest fixtures failed; is the target installed? \
             `rustup target add wasm32-wasip2`"
        );
    });
    let path = target_dir.join(format!("wasm32-wasip2/release/{name}.wasm"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A runtime and the chaos app (in-process, ROADMAP Z9d), shared by every
/// test in a binary.
pub fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let runtime = Runtime::new(RuntimeConfig {
            max_instances: 64,
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = runtime.load_in_process::<bulkhead_host::in_process::chaos::Chaos>();
        (runtime, app)
    })
}

/// A spec with short timeouts, so failure tests finish quickly.
pub fn spec(init_args: &str) -> InstanceSpec {
    InstanceSpec {
        memory_max: 32 << 20,
        callback_deadline: Duration::from_millis(300),
        mailbox_max: 16,
        message_max_bytes: 1 << 20,
        init_timeout: Duration::from_millis(500),
        shutdown_timeout: Duration::from_millis(300),
        init_args: init_args.as_bytes().to_vec(),
        ..InstanceSpec::new("chaos", "0.1.0")
    }
}

pub fn text(body: &str) -> Envelope {
    Envelope::new("text/plain", body)
}

pub fn body(envelope: &Envelope) -> String {
    String::from_utf8(envelope.body.clone()).unwrap()
}

pub const LONG: Duration = Duration::from_secs(10);

/// A pid as the chaos fixture writes it: `node.serial.incarnation@creation`.
pub fn pid_text(pid: bulkhead_host::Pid) -> String {
    pid.text()
}

/// Ask the instance `command` and return the reply body.
pub async fn ask(instance: &bulkhead_host::InstanceHandle, command: &str) -> String {
    body(&instance.call(text(command), LONG).await.unwrap())
}

/// Ask `command` until the reply satisfies `done`, for up to 2 seconds.
pub async fn ask_until(
    instance: &bulkhead_host::InstanceHandle,
    command: &str,
    done: impl Fn(&str) -> bool,
) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let reply = ask(instance, command).await;
        if done(&reply) || std::time::Instant::now() > deadline {
            return reply;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The counter fixture at `version` (1, 2 or 3), each built with its own
/// feature into its own target directory, since features unify within one
/// build.
pub fn counter(version: u8) -> Vec<u8> {
    static BUILT: [OnceLock<()>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    let root = workspace_root();
    let target_dir = root.join(format!("target/fixtures/counter-v{version}"));
    BUILT[usize::from(version - 1)].get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.current_dir(&root).args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip2",
            "-p",
            "bulkhead-fixture-counter",
            "--target-dir",
        ]);
        cmd.arg(&target_dir);
        if version > 1 {
            cmd.args(["--features", &format!("v{version}")]);
        }
        let status = cmd
            .status()
            .expect("run cargo to build the counter fixture");
        assert!(status.success(), "building counter v{version} failed");
    });
    let path = target_dir.join("wasm32-wasip2/release/bulkhead_fixture_counter.wasm");
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A manifest for app `name` with this `appserver` section, unsigned (for
/// app sets built directly on a runtime).
pub fn app_manifest(name: &str, appserver: serde_json::Value) -> AppManifest {
    AppManifest {
        app: AppInfo {
            name: name.into(),
            version: "1.0.0".parse().unwrap(),
            description: None,
            authors: vec![],
            license: None,
            repository: None,
        },
        component: ComponentRef {
            path: String::new(),
            checksum: String::new(),
            world: "bulkhead:runtime/app@0.1.0".into(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(&appserver).unwrap(),
    }
}

/// The conformance app built as a native program (its `native` feature),
/// on first use, into its own target directory.
pub fn native_conformance() -> &'static PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = workspace_root();
        let target = root.join("target/fixtures/native-conformance");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "bulkhead-fixture-conformance",
                "--features",
                "native",
                "--bin",
                "bulkhead-fixture-conformance",
                "--target-dir",
            ])
            .arg(&target)
            .status()
            .expect("run cargo to build the native conformance app");
        assert!(
            status.success(),
            "building the native conformance app failed"
        );
        target.join("debug/bulkhead-fixture-conformance")
    })
}

/// `bulkhead-runner`, the native fixture executable and the rustbridge
/// fixture plugin, built natively on first use into their own target
/// directory.
pub fn native_fixtures() -> &'static (PathBuf, PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = workspace_root();
        let target = root.join("target/fixtures/native");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "bulkhead-native",
                "-p",
                "bulkhead-fixture-native",
                "-p",
                "bulkhead-fixture-plugin",
                "--target-dir",
            ])
            .arg(&target)
            .status()
            .expect("run cargo to build native fixtures");
        assert!(status.success(), "building native fixtures failed");
        let debug = target.join("debug");
        (
            debug.join("bulkhead-runner"),
            debug.join("bulkhead-fixture-native"),
            debug.join("libbulkhead_fixture_plugin.so"),
        )
    })
}

/// The SDK fixture built as a native program (ROADMAP Z9d), once per test
/// binary: the services tests run its commands through the SDK's native
/// build.
pub fn native_sdk() -> &'static PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = workspace_root();
        let target = root.join("target/fixtures/native-sdk");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = Command::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "bulkhead-fixture-sdk",
                "--features",
                "native",
            ])
            .args(["--bin", "bulkhead-fixture-sdk", "--target-dir"])
            .arg(&target)
            .status()
            .unwrap()
            .success();
        assert!(ok, "building the native SDK fixture failed");
        target.join("debug/bulkhead-fixture-sdk")
    })
}

/// Native config with the test-built `bulkhead-runner`.
pub fn native_config() -> bulkhead_host::NativeConfig {
    let mut native = bulkhead_host::NativeConfig::default();
    native.runner = Some(native_fixtures().0.clone());
    native
}

/// The SDK fixture, native, on `rt` (which needs [`native_config`]).
pub fn sdk_app(rt: &Runtime) -> LoadedApp {
    rt.load_native(bulkhead_host::NativeCode {
        path: native_sdk().clone(),
        kind: bulkhead_bundle::manifest::NativeProgram::Executable,
    })
}
