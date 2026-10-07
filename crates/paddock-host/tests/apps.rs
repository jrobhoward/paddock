//! Running sets of apps: start order, root supervisors, pools, routing, and
//! Phase 1 exit criteria 1 and 2, against the chaos fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::{LONG, ask, ask_until, body, pid_text, shared, spec, text};
use paddock_host::{AppDeployment, AppStatus, CallError, Envelope, ExitReason, HostError, Pid};
use paddock_manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};
use serde_json::json;

fn deployment(name: &str, version: &str, appserver: serde_json::Value) -> AppDeployment {
    let (_, app) = shared();
    AppDeployment {
        manifest: AppManifest {
            app: AppInfo {
                name: name.into(),
                version: version.parse().unwrap(),
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
        },
        app: app.clone(),
    }
}

fn fast(mut extra: serde_json::Value) -> serde_json::Value {
    let base = json!({
        "supervision": { "intensity": 2, "backoff": { "initial_ms": 10, "max_ms": 20 } },
        "limits": { "callback_deadline_ms": 300 },
        "lifecycle": { "init_timeout_ms": 500, "shutdown_timeout_ms": 300 }
    });
    let (Some(e), serde_json::Value::Object(b)) = (extra.as_object_mut(), base) else {
        return extra;
    };
    for (k, v) in b {
        e.entry(k).or_insert(v);
    }
    extra
}

fn pid_of(reply: &str) -> Pid {
    let parts: Vec<u64> = reply.split('.').map(|p| p.parse().unwrap()).collect();
    Pid {
        node: u32::try_from(parts[0]).unwrap(),
        id: parts[1],
        incarnation: u32::try_from(parts[2]).unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_apps____dependencies____providers_start_first_and_stop_last() {
    let (rt, _) = shared();
    let apps = rt
        .start_apps(vec![
            deployment(
                "ord-web",
                "1.0.0",
                fast(json!({ "dependencies": [{ "app": "ord-orders", "version": "^1" }] })),
            ),
            deployment("ord-orders", "1.4.0", fast(json!({}))),
        ])
        .await
        .unwrap();
    let web = apps.app("ord-web").unwrap().instances()[0].clone();
    let orders = apps.app("ord-orders").unwrap().instances()[0].clone();
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    for (name, h) in [("web", web.clone()), ("orders", orders.clone())] {
        let order = order.clone();
        tokio::spawn(async move {
            h.wait().await;
            order.lock().unwrap().push(name);
        });
    }

    apps.stop().await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert_eq!(apps.names(), ["ord-orders", "ord-web"]);
    assert!(orders.pid().id < web.pid().id);
    assert_eq!(*order.lock().unwrap(), vec!["web", "orders"]);
    assert_eq!(apps.status("ord-web"), Some(AppStatus::Stopped));
    assert_eq!(apps.status("ord-orders"), Some(AppStatus::Stopped));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_apps____missing_dependency____refused_before_anything_starts() {
    let (rt, _) = shared();

    let result = rt
        .start_apps(vec![deployment(
            "miss-web",
            "1.0.0",
            json!({ "dependencies": [{ "app": "miss-orders", "version": "^1" }] }),
        )])
        .await;

    assert!(
        matches!(result, Err(HostError::InvalidApps(_))),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool____round_robin____calls_rotate_through_every_instance() {
    let (rt, _) = shared();
    let apps = rt
        .start_apps(vec![deployment(
            "rr-pool",
            "1.0.0",
            fast(json!({ "instances": { "count": 3 } })),
        )])
        .await
        .unwrap();
    let app = apps.app("rr-pool").unwrap();

    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(body(&app.call(text("self"), LONG).await.unwrap()));
    }

    assert_eq!(app.instances().len(), 3);
    assert_eq!(seen.iter().collect::<BTreeSet<_>>().len(), 3);
    assert_eq!(seen[0..3], seen[3..6]);
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool____hash_routing____same_key_same_instance_in_json_and_cbor() {
    let (rt, _) = shared();
    let apps = rt
        .start_apps(vec![deployment(
            "hash-pool",
            "1.0.0",
            fast(json!({ "instances": { "count": 4, "routing": "hash:customer_id" } })),
        )])
        .await
        .unwrap();
    let app = apps.app("hash-pool").unwrap();
    let json_msg =
        |id: &str| Envelope::new("application/json", format!(r#"{{"customer_id":"{id}"}}"#));
    let mut cbor_body = Vec::new();
    let map = ciborium::Value::Map(vec![(
        ciborium::Value::Text("customer_id".into()),
        ciborium::Value::Text("c-42".into()),
    )]);
    ciborium::into_writer(&map, &mut cbor_body).unwrap();

    let picks: BTreeSet<Pid> = (0..5)
        .map(|_| app.route(&json_msg("c-42")).unwrap().pid())
        .collect();
    let cbor_pick = app
        .route(&Envelope::new("application/cbor", cbor_body))
        .unwrap()
        .pid();
    let spread: BTreeSet<Pid> = (0..50)
        .map(|i| app.route(&json_msg(&format!("c-{i}"))).unwrap().pid())
        .collect();

    assert_eq!(picks.len(), 1);
    assert_eq!(picks.iter().next().copied(), Some(cbor_pick));
    assert!(spread.len() > 1);
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whereis____app_name_from_guest____resolves_to_a_pool_member() {
    let (rt, app) = shared();
    let apps = rt
        .start_apps(vec![deployment(
            "wh-pool",
            "1.0.0",
            fast(json!({ "instances": { "count": 2 } })),
        )])
        .await
        .unwrap();
    let members: BTreeSet<Pid> = apps
        .app("wh-pool")
        .unwrap()
        .instances()
        .iter()
        .map(|h| h.pid())
        .collect();
    let caller = rt.spawn(app, spec("")).unwrap();

    let found = pid_of(&ask(&caller, "whereis:wh-pool").await);

    assert!(members.contains(&found));
    caller.kill();
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_1____instance_killed____restarted_links_and_monitors_told_stale_pid_noproc()
{
    let (rt, app) = shared();
    let apps = rt
        .start_apps(vec![deployment("ec1-app", "1.0.0", fast(json!({})))])
        .await
        .unwrap();
    let handle = apps.app("ec1-app").unwrap();
    let old = handle.instances()[0].clone();
    let watcher = rt.spawn(app, spec("trap-exits")).unwrap();
    ask(&watcher, &format!("link:{}", pid_text(old.pid()))).await;
    let reference = ask(&watcher, &format!("monitor:{}", pid_text(old.pid()))).await;

    old.kill();
    let mut status = handle.supervisor().unwrap().subscribe_children();
    let new_pid = loop {
        if let Some(p) = status.borrow_and_update()[0]
            .pid()
            .filter(|p| *p != old.pid())
        {
            break p;
        }
        tokio::time::timeout(LONG, status.changed())
            .await
            .unwrap()
            .unwrap();
    };
    let infos = ask_until(&watcher, "infos", |s| s.contains("down")).await;
    let stale = rt.call(old.pid(), text("echo:x"), LONG).await;

    assert_eq!(new_pid.id, old.pid().id);
    assert_eq!(new_pid.incarnation, old.pid().incarnation + 1);
    let a = pid_text(old.pid());
    assert_eq!(
        infos,
        format!("exit:{a}:killed;down:{reference}:{a}:killed")
    );
    assert_eq!(stale, Err(CallError::Noproc));
    assert_eq!(handle.status(), AppStatus::Running);
    watcher.kill();
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_2____init_traps____app_failed_with_reason_and_dependents_not_started() {
    let (rt, _) = shared();
    let apps = rt
        .start_apps(vec![
            deployment(
                "ec2-bad",
                "1.0.0",
                fast(json!({ "lifecycle": { "start_args": "trap", "init_timeout_ms": 500 } })),
            ),
            deployment(
                "ec2-dependent",
                "1.0.0",
                fast(json!({ "dependencies": [{ "app": "ec2-bad", "version": "*" }] })),
            ),
            deployment("ec2-fine", "1.0.0", fast(json!({}))),
        ])
        .await
        .unwrap();

    let bad = apps.status("ec2-bad").unwrap();
    let dependent = apps.status("ec2-dependent").unwrap();

    let AppStatus::Failed(ExitReason::Crashed(why)) = &bad else {
        panic!("expected failed, got {bad:?}");
    };
    assert!(
        why.contains("max restart intensity (2 in 10000 ms)"),
        "{why}"
    );
    assert!(why.contains("last exit: instance-1 crashed: trap"), "{why}");
    assert_eq!(
        dependent,
        AppStatus::Failed(ExitReason::Crashed(
            "dependency ec2-bad is not running".into()
        ))
    );
    assert_eq!(apps.status("ec2-fine"), Some(AppStatus::Running));
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_app____restarts_exhausted_later____status_becomes_failed() {
    let (rt, _) = shared();
    let apps = rt
        .start_apps(vec![deployment("late-fail", "1.0.0", fast(json!({})))])
        .await
        .unwrap();
    let app = apps.app("late-fail").unwrap();
    let mut status = app.subscribe_status();

    for _ in 0..3 {
        if let Some(i) = app.instances().first() {
            i.kill();
            i.wait().await;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let reached =
        tokio::time::timeout(LONG, status.wait_for(|s| matches!(s, AppStatus::Failed(_)))).await;

    assert!(reached.is_ok(), "status stayed {:?}", app.status());
    apps.stop().await;
    assert!(matches!(app.status(), AppStatus::Failed(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervision_children____other_app____runs_under_this_apps_root() {
    let (rt, _) = shared();
    let apps = rt
        .start_apps(vec![
            deployment("sc-worker", "1.0.0", fast(json!({ "instances": { "count": 1 } }))),
            deployment(
                "sc-boss",
                "1.0.0",
                fast(json!({ "supervision": { "intensity": 2, "children": [{ "id": "helper", "app": "sc-worker" }] } })),
            ),
        ])
        .await
        .unwrap();

    let children = apps
        .app("sc-boss")
        .unwrap()
        .supervisor()
        .unwrap()
        .children();

    let shape: Vec<(String, Option<String>)> = children
        .iter()
        .map(|c| (c.id.clone(), c.app.clone()))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("instance-1".into(), Some("sc-boss".into())),
            ("helper".into(), Some("sc-worker".into())),
        ]
    );
    assert_eq!(apps.app("sc-boss").unwrap().instances().len(), 1);
    apps.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simple_one_for_one_app____instances_count____starts_that_many_from_template() {
    let (rt, _) = shared();
    let apps = rt
        .start_apps(vec![deployment(
            "sofo-app",
            "1.0.0",
            fast(json!({
                "instances": { "count": 3 },
                "supervision": { "strategy": "simple_one_for_one", "intensity": 2, "children": [{ "id": "w", "app": "sofo-app" }] }
            })),
        )])
        .await
        .unwrap();
    let app = apps.app("sofo-app").unwrap();

    let ids: Vec<String> = app
        .supervisor()
        .unwrap()
        .children()
        .iter()
        .map(|c| c.id.clone())
        .collect();

    assert_eq!(ids, vec!["w-1", "w-2", "w-3"]);
    assert_eq!(app.instances().len(), 3);
    assert_eq!(app.status(), AppStatus::Running);
    apps.stop().await;
}
