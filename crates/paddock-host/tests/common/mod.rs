//! Builds guest fixtures and sets up runtimes for tests.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

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
        app: "chaos".into(),
        version: "0.1.0".into(),
        memory_max: 32 << 20,
        table_max: None,
        callback_deadline: Duration::from_millis(300),
        mailbox_max: 16,
        message_max_bytes: 1 << 20,
        registered_names: Vec::new(),
        init_timeout: Duration::from_millis(500),
        shutdown_timeout: Duration::from_millis(300),
        init_args: init_args.as_bytes().to_vec(),
    }
}

pub fn text(body: &str) -> Envelope {
    Envelope::new("text/plain", body)
}

pub fn body(envelope: &Envelope) -> String {
    String::from_utf8(envelope.body.clone()).unwrap()
}

pub const LONG: Duration = Duration::from_secs(10);

/// A pid as the chaos fixture writes it: `node.id.incarnation`.
pub fn pid_text(pid: paddock_host::Pid) -> String {
    format!("{}.{}.{}", pid.node, pid.id, pid.incarnation)
}
