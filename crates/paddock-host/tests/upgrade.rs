//! Hot upgrade: instance-level snapshot upgrades, app rollouts in snapshot and
//! restart mode, revert, approval and rollback. Covers Phase 1 exit
//! criterion 6.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use common::{LONG, ask, ask_until, counter, pid_text, shared, spec, text};
use paddock_host::{
    AppDeployment, CallError, ExitReason, InstanceHandle, InstanceSpec, LoadedApp, UpgradeError,
    UpgradeOptions,
};
use paddock_manifest::{
    AppInfo, AppManifest, AppSpec, Capability, ComponentRef, Provenance, UpgradeMode,
};
use serde_json::json;

fn counter_app(version: u8) -> &'static LoadedApp {
    static APPS: [OnceLock<LoadedApp>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    APPS[usize::from(version - 1)].get_or_init(|| shared().0.load(&counter(version)).unwrap())
}

fn counter_spec(init_args: &str, names: &[&str]) -> InstanceSpec {
    let mut s = spec(init_args);
    s.app = "counter".into();
    s.registered_names = names.iter().map(ToString::to_string).collect();
    s
}

async fn incs(h: &InstanceHandle, n: usize) {
    for _ in 0..n {
        ask(h, "inc").await;
    }
}

// --- instance level ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_upgrade____v1_to_v2____state_pid_and_name_kept() {
    let (rt, _) = shared();
    let h = rt
        .spawn(
            counter_app(1),
            counter_spec("register:up-name", &["up-name"]),
        )
        .unwrap();
    incs(&h, 5).await;

    let pause = h
        .upgrade(counter_app(2), counter_spec("", &["up-name"]), "1.0.0")
        .await
        .unwrap();
    let after = ask(&h, "get").await;
    let next = ask(&h, "inc").await;

    assert_eq!(after, "2.0.0 count=5 history=");
    assert_eq!(next, "6");
    assert_eq!(ask(&h, "self").await, pid_text(h.pid()));
    assert_eq!(rt.whereis("up-name").unwrap().pid(), h.pid());
    assert!(pause < Duration::from_secs(1), "{pause:?}");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_upgrade____load_snapshot_fails____old_version_keeps_running() {
    let (rt, _) = shared();
    let h = rt.spawn(counter_app(1), counter_spec("", &[])).unwrap();
    incs(&h, 2).await;
    h.upgrade(counter_app(2), counter_spec("", &[]), "1.0.0")
        .await
        .unwrap();
    incs(&h, 2).await;

    let result = h
        .upgrade(counter_app(3), counter_spec("", &[]), "2.0.0")
        .await;

    assert_eq!(
        result,
        Err(UpgradeError::LoadFailed(
            "v3 cannot read snapshots from 2.0.0".into()
        ))
    );
    assert_eq!(ask(&h, "get").await, "2.0.0 count=4 history=3,4");
    assert_eq!(h.state(), paddock_host::LifecycleState::Active);
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn instance_upgrade____under_concurrent_traffic____no_message_lost() {
    let (rt, _) = shared();
    let h = rt.spawn(counter_app(1), counter_spec("", &[])).unwrap();
    let senders: Vec<_> = (0..4)
        .map(|_| {
            let h = h.clone();
            tokio::spawn(async move {
                for _ in 0..150 {
                    h.call(text("inc"), LONG).await.unwrap();
                    tokio::time::sleep(Duration::from_micros(300)).await;
                }
            })
        })
        .collect();

    tokio::time::sleep(Duration::from_millis(20)).await;
    h.upgrade(counter_app(2), counter_spec("", &[]), "1.0.0")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let broken = h
        .upgrade(counter_app(3), counter_spec("", &[]), "2.0.0")
        .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    h.upgrade(counter_app(2), counter_spec("", &[]), "2.0.0")
        .await
        .unwrap();
    for s in senders {
        s.await.unwrap();
    }
    let total = ask(&h, "get").await;

    assert!(matches!(broken, Err(UpgradeError::LoadFailed(_))));
    assert!(total.starts_with("2.0.0 count=600 "), "{total}");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_upgrade____links_survive_the_swap() {
    let (rt, chaos) = shared();
    let h = rt.spawn(counter_app(1), counter_spec("", &[])).unwrap();
    let peer = rt.spawn(chaos, spec("")).unwrap();
    ask(&peer, &format!("link:{}", pid_text(h.pid()))).await;

    h.upgrade(counter_app(2), counter_spec("", &[]), "1.0.0")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let peer_alive = peer.exit_reason().is_none();
    h.call(text("trap"), LONG).await.unwrap_err();

    assert!(peer_alive);
    assert!(matches!(peer.wait().await, ExitReason::Crashed(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_upgrade____timers_and_deferred_callers_carried_over() {
    let (rt, chaos) = shared();
    let h = rt.spawn(chaos, spec("")).unwrap();
    ask(&h, "timer:150:after-upgrade").await;
    let waiting = tokio::spawn({
        let h = h.clone();
        async move { h.call(text("defer"), LONG).await }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;

    h.upgrade(chaos, spec(""), "0.1.0").await.unwrap();
    ask(&h, "answer:answered-by-v2").await;
    let reply = waiting.await.unwrap().unwrap();
    let fired = ask_until(&h, "timers", |s| !s.is_empty()).await;

    assert_eq!(String::from_utf8(reply.body).unwrap(), "answered-by-v2");
    assert_eq!(fired, "after-upgrade");
    h.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_upgrade____after_exit____not_running() {
    let (rt, _) = shared();
    let h = rt.spawn(counter_app(1), counter_spec("", &[])).unwrap();
    h.kill();
    h.wait().await;

    let result = h
        .upgrade(counter_app(2), counter_spec("", &[]), "1.0.0")
        .await;

    assert_eq!(result, Err(UpgradeError::NotRunning));
}

// --- app level -----------------------------------------------------------------

/// The app's first running instance, waiting out a restart.
async fn running(app: &paddock_host::AppHandle) -> InstanceHandle {
    running_at(app, 0).await
}

async fn running_at(app: &paddock_host::AppHandle, index: usize) -> InstanceHandle {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let found = app.instances().get(index).cloned();
        if let Some(h) = found.filter(|h| h.state() == paddock_host::LifecycleState::Active) {
            return h;
        }
        assert!(std::time::Instant::now() < deadline, "no running instance");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn deployment(name: &str, version: u8, appserver: serde_json::Value) -> AppDeployment {
    let mut spec = appserver;
    let base = json!({
        "supervision": { "intensity": 3, "backoff": { "initial_ms": 10, "max_ms": 20 } },
        "limits": { "callback_deadline_ms": 1000 },
        "lifecycle": { "init_timeout_ms": 1000, "shutdown_timeout_ms": 300 }
    });
    if let (Some(s), serde_json::Value::Object(b)) = (spec.as_object_mut(), base) {
        for (k, v) in b {
            s.entry(k).or_insert(v);
        }
    }
    AppDeployment::new(
        AppManifest {
            app: AppInfo {
                name: name.into(),
                version: format!("{version}.0.0").parse().unwrap(),
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
            spec: AppSpec::from_json_value(&spec).unwrap(),
        },
        counter_app(version).clone(),
    )
}

fn snapshot_mode(from: &str) -> serde_json::Value {
    json!({ "mode": "snapshot", "snapshot_version": 1, "from_versions": [from] })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exit_criterion_6____v1_to_v2_keeps_identity_and_broken_v3_reverts_losing_nothing() {
    let (rt, chaos) = shared();
    let name = "ec6-counter";
    let v1 = json!({ "registered_names": [name], "lifecycle": { "start_args": format!("register:{name}") } });
    let apps = rt
        .start_apps(vec![deployment(name, 1, v1.clone())])
        .await
        .unwrap();
    let app = apps.app(name).unwrap().clone();
    let instance = app.instances()[0].clone();
    let peer = rt.spawn(chaos, spec("")).unwrap();
    ask(&peer, &format!("link:{}", pid_text(instance.pid()))).await;
    let traffic: Vec<_> = (0..3)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move {
                for _ in 0..100 {
                    app.call(text("inc"), LONG).await.unwrap();
                    tokio::time::sleep(Duration::from_micros(500)).await;
                }
            })
        })
        .collect();

    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut v2 = v1.clone();
    v2["upgrade"] = snapshot_mode("^1");
    let up = apps
        .upgrade(
            name,
            deployment(name, 2, v2.clone()),
            &UpgradeOptions::default(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut v3 = v1.clone();
    v3["upgrade"] = snapshot_mode("^2");
    let broken = apps
        .upgrade(name, deployment(name, 3, v3), &UpgradeOptions::default())
        .await;
    for t in traffic {
        t.await.unwrap();
    }
    let state = ask(&instance, "get").await;

    assert_eq!(up.mode, UpgradeMode::Snapshot);
    assert_eq!(up.instances, vec![instance.pid()]);
    let Err(UpgradeError::Reverted(why)) = &broken else {
        panic!("expected reverted, got {broken:?}");
    };
    assert!(why.contains("v3 cannot read snapshots from 2.0.0"), "{why}");
    assert_eq!(app.version().to_string(), "2.0.0");
    assert!(state.starts_with("2.0.0 count=300 history="), "{state}");
    assert_eq!(rt.whereis(name).unwrap().pid(), instance.pid());
    assert!(peer.exit_reason().is_none());

    instance.call(text("trap"), LONG).await.unwrap_err();
    assert!(matches!(peer.wait().await, ExitReason::Crashed(_)));
    let restarted = running(&app).await;
    assert_eq!(restarted.pid().incarnation, instance.pid().incarnation + 1);
    assert_eq!(ask(&restarted, "get").await, "2.0.0 count=0 history=");
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade____restart_mode____same_ids_next_incarnation_fresh_state() {
    let (rt, _) = shared();
    let name = "up-restart";
    let apps = rt
        .start_apps(vec![deployment(
            name,
            1,
            json!({ "instances": { "count": 2 } }),
        )])
        .await
        .unwrap();
    let app = apps.app(name).unwrap();
    let before: Vec<_> = app.instances().iter().map(InstanceHandle::pid).collect();
    for h in app.instances() {
        incs(&h, 3).await;
    }

    let report = apps
        .upgrade(
            name,
            deployment(name, 2, json!({ "instances": { "count": 2 } })),
            &UpgradeOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(report.mode, UpgradeMode::Restart);
    let after: Vec<_> = app.instances().iter().map(InstanceHandle::pid).collect();
    for (b, a) in before.iter().zip(&after) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.incarnation, b.incarnation + 1);
    }
    for h in app.instances() {
        assert_eq!(ask(&h, "get").await, "2.0.0 count=0 history=");
    }
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade____restarts_exceed_threshold____every_instance_reverted() {
    let (rt, _) = shared();
    let name = "up-threshold";
    let apps = rt
        .start_apps(vec![deployment(
            name,
            1,
            json!({ "instances": { "count": 2 } }),
        )])
        .await
        .unwrap();
    let app = apps.app(name).unwrap().clone();
    for h in app.instances() {
        incs(&h, 2).await;
    }
    // Built first: compiling it must not eat into the observe window.
    let v2 = deployment(
        name,
        2,
        json!({ "instances": { "count": 2 }, "upgrade": snapshot_mode("^1") }),
    );
    let victim = app.instances()[1].clone();
    let crash = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        victim.kill();
    });
    let options = UpgradeOptions {
        batch_size: 2,
        observe: Duration::from_millis(300),
        max_restarts: 0,
        ..UpgradeOptions::default()
    };

    let result = apps.upgrade(name, v2, &options).await;
    crash.await.unwrap();

    let Err(UpgradeError::Reverted(why)) = &result else {
        panic!("expected reverted, got {result:?}");
    };
    assert!(why.contains("restarts during the rollout"), "{why}");
    assert_eq!(app.version().to_string(), "1.0.0");
    let survivor = ask(&app.instances()[0], "get").await;
    assert_eq!(survivor, "1.0.0 count=2");
    let replaced = ask(&running_at(&app, 1).await, "get").await;
    assert!(replaced.starts_with("1.0.0"), "{replaced}");
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade____new_capability____needs_approval() {
    let (rt, _) = shared();
    let name = "up-approve";
    let apps = rt
        .start_apps(vec![deployment(name, 1, json!({}))])
        .await
        .unwrap();
    let v2 = || deployment(name, 2, json!({ "capabilities": { "wasi:logging": {} } }));

    let refused = apps.upgrade(name, v2(), &UpgradeOptions::default()).await;
    let approved = apps
        .upgrade(
            name,
            v2(),
            &UpgradeOptions {
                approve: true,
                ..UpgradeOptions::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        refused,
        Err(UpgradeError::NeedsApproval(vec![Capability::Logging]))
    );
    assert_eq!(approved.approved, vec![Capability::Logging]);
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback____after_snapshot_upgrade____previous_version_with_state() {
    let (rt, _) = shared();
    let name = "up-rollback";
    let v1 = json!({ "upgrade": { "mode": "snapshot", "snapshot_version": 1 } });
    let apps = rt.start_apps(vec![deployment(name, 1, v1)]).await.unwrap();
    let app = apps.app(name).unwrap();
    let h = app.instances()[0].clone();
    incs(&h, 4).await;
    apps.upgrade(
        name,
        deployment(name, 2, json!({ "upgrade": snapshot_mode("^1") })),
        &UpgradeOptions::default(),
    )
    .await
    .unwrap();
    incs(&h, 1).await;

    let report = apps
        .rollback(name, &UpgradeOptions::default())
        .await
        .unwrap();

    assert_eq!(
        (report.from.to_string(), report.to.to_string()),
        ("2.0.0".into(), "1.0.0".into())
    );
    assert_eq!(report.mode, UpgradeMode::Snapshot);
    assert_eq!(ask(&h, "get").await, "1.0.0 count=5");
    assert_eq!(app.version().to_string(), "1.0.0");
    assert_eq!(
        app.previous_version().map(|v| v.to_string()),
        Some("2.0.0".into())
    );
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback____nothing_to_return_to____no_previous() {
    let (rt, _) = shared();
    let apps = rt
        .start_apps(vec![deployment("up-noprev", 1, json!({}))])
        .await
        .unwrap();

    let result = apps.rollback("up-noprev", &UpgradeOptions::default()).await;
    let unknown = apps.rollback("nobody", &UpgradeOptions::default()).await;

    assert_eq!(result, Err(UpgradeError::NoPrevious("up-noprev".into())));
    assert_eq!(unknown, Err(UpgradeError::UnknownApp("nobody".into())));
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_pid_after_restart_mode_upgrade____noproc() {
    let (rt, _) = shared();
    let name = "up-stale";
    let apps = rt
        .start_apps(vec![deployment(name, 1, json!({}))])
        .await
        .unwrap();
    let old = apps.app(name).unwrap().instances()[0].pid();

    apps.upgrade(
        name,
        deployment(name, 2, json!({})),
        &UpgradeOptions::default(),
    )
    .await
    .unwrap();
    let result = rt.call(old, text("get"), LONG).await;

    assert_eq!(result, Err(CallError::Noproc));
    apps.stop().await;
}
