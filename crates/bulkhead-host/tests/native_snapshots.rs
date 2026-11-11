//! Snapshots for native-process apps: the fixture executable's
//! `save_snapshot` and `load_snapshot` over the protocol, for reading an
//! instance's state, snapshot upgrades and moves, each into a
//! new process; and a rustbridge plugin, which takes none.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::{
    AppDeployment, AppHandle, ExitReason, InstanceHandle, InstanceSpec, LifecycleState, LoadedApp,
    NativeCode, NativeConfig, Runtime, RuntimeConfig, SnapshotError, UpgradeError,
};
use common::{app_manifest, ask, native_fixtures};
use serde_json::json;

fn runtime() -> Runtime {
    let mut native = NativeConfig::default();
    native.runner = Some(native_fixtures().0.clone());
    Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        ..RuntimeConfig::default()
    })
    .unwrap()
}

fn executable(rt: &Runtime) -> LoadedApp {
    rt.load_native(NativeCode {
        path: native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    })
}

fn spec(version: &str) -> InstanceSpec {
    InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_millis(500),
        init_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(1),
        ..InstanceSpec::new("native-snap", version)
    }
}

/// Whether the process with this id has gone, within a few seconds.
async fn gone(os_pid: &str) -> bool {
    let path = format!("/proc/{os_pid}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::path::Path::new(&path).exists() {
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_snapshot____notes____what_save_snapshot_wrote_and_kept_running() {
    let rt = runtime();
    let h = rt.spawn(&executable(&rt), spec("1.0.0")).unwrap();
    ask(&h, "note a").await;
    ask(&h, "note b").await;

    let snapshot = h.snapshot().await.unwrap();
    let notes = ask(&h, "notes").await;

    assert_eq!(snapshot, b"a\nb");
    assert_eq!(notes, "a,b");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_snapshot____save_fails____instance_crashed_with_why() {
    let rt = runtime();
    let h = rt.spawn(&executable(&rt), spec("1.0.0")).unwrap();
    ask(&h, "fail-save").await;
    let why = ExitReason::Crashed("save_snapshot failed: asked to fail".into());

    let snapshot = h.snapshot().await;
    let reason = h.wait().await;

    assert_eq!(snapshot, Err(SnapshotError::Failed(why.clone())));
    assert_eq!(reason, why);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_upgrade____snapshot____new_process_with_the_pid_and_state() {
    let rt = runtime();
    let app = executable(&rt);
    let h = rt.spawn(&app, spec("1.0.0")).unwrap();
    ask(&h, "note kept").await;
    let before = ask(&h, "os-pid").await;
    let me = ask(&h, "self").await;

    let pause = h.upgrade(&app, spec("2.0.0"), "1.0.0").await;
    let after = ask(&h, "os-pid").await;

    assert!(pause.is_ok(), "{pause:?}");
    assert_eq!(ask(&h, "notes").await, "kept");
    assert_eq!(ask(&h, "loaded-from").await, "1.0.0");
    assert_eq!(ask(&h, "self").await, me);
    assert_eq!(h.version(), "2.0.0");
    assert_ne!(after, before);
    assert!(gone(&before).await, "the old process is still running");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_upgrade____snapshot_refused____old_process_keeps_running() {
    let rt = runtime();
    let app = executable(&rt);
    let h = rt.spawn(&app, spec("1.0.0")).unwrap();
    ask(&h, "note refuse").await;
    let before = ask(&h, "os-pid").await;

    let result = h.upgrade(&app, spec("2.0.0"), "1.0.0").await;

    assert_eq!(
        result,
        Err(UpgradeError::LoadFailed("asked to refuse".into()))
    );
    assert_eq!(ask(&h, "os-pid").await, before);
    assert_eq!(ask(&h, "notes").await, "refuse");
    assert_eq!(h.version(), "1.0.0");
    assert_eq!(h.state(), LifecycleState::Active);
    h.kill();
}

fn deployment(rt: &Runtime, name: &str, version: &str) -> AppDeployment {
    let mut manifest = app_manifest(
        name,
        json!({
            "kind": "native-process",
            "native": { "program": "executable" },
            "limits": { "callback_deadline_ms": 500, "memory_max": "64MiB" },
            "upgrade": { "mode": "snapshot", "snapshot_version": 1, "from_versions": ["^1"] }
        }),
    );
    manifest.app.version = version.parse().unwrap();
    AppDeployment::new(manifest, executable(rt))
}

async fn running(app: &AppHandle) -> InstanceHandle {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found = app.instances().first().cloned();
        if let Some(h) = found.filter(|h| h.state() == LifecycleState::Active) {
            return h;
        }
        assert!(Instant::now() < deadline, "no running instance");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_move_out____then_add_moved____state_on_the_new_instance() {
    let rt = runtime();
    let name = "native-moved";
    let apps = rt
        .start_apps(vec![deployment(&rt, name, "1.0.0")])
        .await
        .unwrap();
    let old = running(&apps.app(name).unwrap()).await;
    ask(&old, "note kept").await;
    let there = rt.start_apps(Vec::new()).await.unwrap();

    let handover = apps.move_out(name).await.unwrap();
    let old_exit = old.wait().await;
    let moved = there
        .add_moved(deployment(&rt, name, "1.1.0"), handover.clone())
        .await
        .unwrap();
    let new = running(&moved).await;

    assert_eq!(handover.version, "1.0.0");
    assert_eq!(handover.snapshot, b"kept");
    assert_eq!(old_exit, ExitReason::Normal);
    assert_ne!(new.pid(), old.pid());
    assert_eq!(ask(&new, "notes").await, "kept");
    assert_eq!(ask(&new, "loaded-from").await, "1.0.0");
    there.stop().await;
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn library____snapshot_and_upgrade____refused() {
    let rt = runtime();
    let app = rt.load_native(NativeCode {
        path: native_fixtures().2.clone(),
        kind: NativeProgram::Library,
    });
    let h = rt.spawn(&app, spec("1.0.0")).unwrap();

    let snapshot = h.snapshot().await;
    let upgrade = h.upgrade(&app, spec("2.0.0"), "1.0.0").await;

    assert_eq!(snapshot, Err(SnapshotError::NotSupported));
    assert!(
        matches!(upgrade, Err(UpgradeError::Invalid(_))),
        "{upgrade:?}"
    );
    assert!(h.exit_reason().is_none());
    h.kill();
}
