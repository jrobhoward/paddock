//! Builds guest fixtures and sets up runtimes for tests.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

pub mod pg;

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use paddock_bundle::manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};
use paddock_host::{Envelope, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The fixture component `name` (`paddock_fixture_chaos`), built for
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
                "paddock-fixture-chaos",
                "-p",
                "paddock-fixture-echo",
                "-p",
                "paddock-fixture-sdk",
                "-p",
                "paddock-fixture-conn",
                "-p",
                "paddock-fixture-http",
                "-p",
                "paddock-fixture-services",
                "--features",
                "paddock-fixture-services/denied",
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

/// A runtime and the chaos app, shared by every test in a binary so the
/// component compiles once.
pub fn shared() -> &'static (Runtime, LoadedApp) {
    static SHARED: OnceLock<(Runtime, LoadedApp)> = OnceLock::new();
    SHARED.get_or_init(|| {
        let runtime = Runtime::new(RuntimeConfig {
            max_instances: 64,
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = runtime.load(&fixture("paddock_fixture_chaos")).unwrap();
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
pub fn pid_text(pid: paddock_host::Pid) -> String {
    pid.text()
}

/// Ask the instance `command` and return the reply body.
pub async fn ask(instance: &paddock_host::InstanceHandle, command: &str) -> String {
    body(&instance.call(text(command), LONG).await.unwrap())
}

/// Ask `command` until the reply satisfies `done`, for up to 2 seconds.
pub async fn ask_until(
    instance: &paddock_host::InstanceHandle,
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
            "paddock-fixture-counter",
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
    let path = target_dir.join("wasm32-wasip2/release/paddock_fixture_counter.wasm");
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
            world: "paddock:runtime/app@0.1.0".into(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(&appserver).unwrap(),
    }
}
