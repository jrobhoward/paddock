//! Native fixtures, and runtimes for tests.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

pub mod pg;
pub mod ports;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub mod privhelper;

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use bulkhead_bundle::manifest::{AppInfo, AppManifest, AppSpec, ProgramRef, Provenance};
use bulkhead_host::{Envelope, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
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
        program: ProgramRef {
            path: String::new(),
            checksum: String::new(),
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
            .args(["build", "-p", "bulkhead-fixture-sdk"])
            .args(["--bin", "bulkhead-fixture-sdk", "--target-dir"])
            .arg(&target)
            .status()
            .unwrap()
            .success();
        assert!(ok, "building the native SDK fixture failed");
        target.join("debug/bulkhead-fixture-sdk")
    })
}

/// Whether native apps run without the runner's sandbox here: on
/// FreeBSD, which has none until ROADMAP FB2, so the runner refuses to
/// start an app unless the node allows it (`landlock_optional`).
pub const NO_SANDBOX: bool = cfg!(target_os = "freebsd");

/// Native config with the test-built `bulkhead-runner`.
pub fn native_config() -> bulkhead_host::NativeConfig {
    let mut native = bulkhead_host::NativeConfig::default();
    native.runner = Some(native_fixtures().0.clone());
    native.landlock_optional = NO_SANDBOX;
    native
}

/// The SDK fixture, native, on `rt` (which needs [`native_config`]).
pub fn sdk_app(rt: &Runtime) -> LoadedApp {
    rt.load_native(bulkhead_host::NativeCode {
        path: native_sdk().clone(),
        kind: bulkhead_bundle::manifest::NativeProgram::Executable,
    })
}
