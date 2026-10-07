//! A node end to end through its admin API: deploy, status, kill, upgrade,
//! rollback, stop/start/remove, restart restore, refusals, logs and the
//! audit log. Covers Phase 1 exit criteria 7 and 9 through a node.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{TestNode, bundle, counter, fixture};
use minisign::KeyPair;
use paddock_api::{ClientError, RolloutParams};
use serde_json::json;

fn fast(mut extra: serde_json::Value) -> serde_json::Value {
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

fn kind(err: &ClientError) -> String {
    match err {
        ClientError::Api(e) => e.kind.clone(),
        other => format!("{other}"),
    }
}

async fn call(node: &TestNode, app: &str, body: &str) -> String {
    let (_, reply) = node
        .client
        .call(app, "text/plain", body.as_bytes().to_vec(), 5_000)
        .await
        .unwrap();
    String::from_utf8(reply).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy____signed_bundle____running_listed_and_callable() {
    let mut n = TestNode::start("").await;
    let b = bundle(
        &n.key,
        "echoer",
        "1.0.0",
        &fixture("paddock_fixture_chaos"),
        fast(json!({ "instances": { "count": 2 } })),
        &[],
    );

    let deployed = n.client.deploy(b).await.unwrap();
    let list = n.client.list().await.unwrap();
    let ps = n.client.ps().await.unwrap();
    let reply = call(&n, "echoer", "echo:hello").await;

    assert_eq!(deployed.app.status, "running");
    assert_eq!(list.len(), 1);
    assert_eq!(
        (
            list[0].name.as_str(),
            list[0].version.as_str(),
            list[0].instances
        ),
        ("echoer", "1.0.0", 2)
    );
    assert_eq!(
        ps.iter()
            .filter(|i| i.app == "echoer" && i.state == "active")
            .count(),
        2
    );
    assert_eq!(reply, "hello");
    assert_eq!(n.events(), ["deploy"]);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill____instance____restarted_with_next_incarnation() {
    let mut n = TestNode::start("").await;
    n.client
        .deploy(bundle(
            &n.key,
            "victim",
            "1.0.0",
            &fixture("paddock_fixture_chaos"),
            fast(json!({})),
            &[],
        ))
        .await
        .unwrap();
    let before = n.client.ps().await.unwrap()[0].pid.clone();

    n.client.kill(&before).await.unwrap();
    let mut after = String::new();
    for _ in 0..200 {
        let ps = n.client.ps().await.unwrap();
        if let Some(i) = ps.iter().find(|i| i.pid != before && i.state == "active") {
            after = i.pid.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let status = n.client.status("victim").await.unwrap();

    let (b, a): (Vec<u64>, Vec<u64>) = (
        before.split('.').map(|x| x.parse().unwrap()).collect(),
        after.split('.').map(|x| x.parse().unwrap()).collect(),
    );
    assert_eq!(a[1], b[1]);
    assert_eq!(a[2], b[2] + 1);
    assert_eq!(status.restarts, 1);
    assert!(n.events().contains(&"kill".to_string()));
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_9____tampered_or_untrusted_bundle____refused_and_audited() {
    let mut n = TestNode::start("").await;
    let stranger = KeyPair::generate_unencrypted_keypair().unwrap();
    let untrusted = bundle(
        &stranger,
        "x",
        "1.0.0",
        &fixture("paddock_fixture_chaos"),
        json!({}),
        &[],
    );
    let mut tampered = bundle(
        &n.key,
        "x",
        "1.0.0",
        &fixture("paddock_fixture_chaos"),
        json!({}),
        &[],
    );
    let at = tampered.len() / 2;
    tampered[at] ^= 0xff;

    let e1 = n.client.deploy(untrusted).await.unwrap_err();
    let e2 = n.client.deploy(tampered).await.unwrap_err();

    assert_eq!(kind(&e1), "refused");
    assert!(e1.to_string().contains("not trusted"), "{e1}");
    assert!(matches!(kind(&e2).as_str(), "refused" | "internal"), "{e2}");
    assert!(n.client.list().await.unwrap().is_empty());
    assert_eq!(n.events(), ["deploy_refused", "deploy_refused"]);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_7____undeclared_import____refused_at_deploy() {
    let mut n = TestNode::start("").await;
    let b = bundle(
        &n.key,
        "sneaky",
        "1.0.0",
        &fixture("paddock_fixture_services"),
        json!({}),
        &[],
    );

    let err = n.client.deploy(b).await.unwrap_err();

    assert_eq!(kind(&err), "refused");
    assert!(
        err.to_string()
            .contains("imports not declared under capabilities"),
        "{err}"
    );
    assert!(err.to_string().contains("wasi:keyvalue/store"), "{err}");
    n.shutdown().await;
}

fn services_manifest() -> serde_json::Value {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_7____declared_but_ungranted____denied_at_call_time() {
    let scratch = tempfile::tempdir().unwrap();
    let policy_dir = tempfile::tempdir().unwrap();
    let policy = policy_dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[defaults]\ngrant = [\"wasi:logging\", \"wasi:config\", \"wasi:filesystem\"]\n",
    )
    .unwrap();
    let mut n = TestNode::start(&format!(
        "policy = {:?}\n[volumes]\nscratch = {:?}\n",
        policy,
        scratch.path()
    ))
    .await;
    let b = bundle(
        &n.key,
        "svc",
        "1.0.0",
        &fixture("paddock_fixture_services"),
        services_manifest(),
        &[("priv/motd.txt", b"from the bundle")],
    );

    let deployed = n.client.deploy(b).await.unwrap();
    let kv = call(&n, "svc", "kv-get:data:k").await;
    let priv_file = call(&n, "svc", "read:/priv/motd.txt").await;
    let status = n.client.status("svc").await.unwrap();

    assert_eq!(deployed.denied, vec!["wasi:keyvalue"]);
    assert_eq!(kv, "err:access-denied");
    assert_eq!(priv_file, "from the bundle");
    assert_eq!(
        status.granted,
        vec!["wasi:logging", "wasi:config", "wasi:filesystem"]
    );
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_overrides_and_logs____node_values_win_and_records_are_kept() {
    let scratch = tempfile::tempdir().unwrap();
    let policy_dir = tempfile::tempdir().unwrap();
    let policy = policy_dir.path().join("policy.toml");
    std::fs::write(&policy, "[apps.svclog]\ngrant = [\"wasi:logging\", \"wasi:config\", \"wasi:keyvalue\", \"wasi:filesystem\"]\n").unwrap();
    let mut n = TestNode::start(&format!(
        "policy = {:?}\n[volumes]\nscratch = {:?}\n[config.svclog]\nmode = \"from-node\"\n",
        policy,
        scratch.path()
    ))
    .await;
    n.client
        .deploy(bundle(
            &n.key,
            "svclog",
            "1.0.0",
            &fixture("paddock_fixture_services"),
            services_manifest(),
            &[],
        ))
        .await
        .unwrap();

    let all = call(&n, "svclog", "config-all").await;
    call(&n, "svclog", "log:warn:disk almost full").await;
    call(&n, "svclog", "print:to stdout").await;
    let mut lines = Vec::new();
    for _ in 0..100 {
        lines = n.client.logs("svclog", 10).await.unwrap();
        if lines.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(all, "greeting=hello;mode=from-node");
    let messages: Vec<(&str, &str)> = lines
        .iter()
        .map(|l| (l.source.as_str(), l.message.as_str()))
        .collect();
    assert_eq!(
        messages,
        vec![("fixture", "disk almost full"), ("stdout", "to stdout")]
    );
    assert_eq!(lines[0].level, "WARN");
    n.shutdown().await;
}

fn counter_manifest(from: Option<&str>) -> serde_json::Value {
    let mut m = fast(json!({}));
    if let Some(f) = from {
        m["upgrade"] = json!({ "mode": "snapshot", "snapshot_version": 1, "from_versions": [f] });
    } else {
        m["upgrade"] = json!({ "mode": "snapshot", "snapshot_version": 1 });
    }
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade_and_rollback____through_the_api____state_kept_and_audited() {
    let mut n = TestNode::start("").await;
    n.client
        .deploy(bundle(
            &n.key,
            "ctr",
            "1.0.0",
            &counter(1),
            counter_manifest(None),
            &[],
        ))
        .await
        .unwrap();
    for _ in 0..3 {
        call(&n, "ctr", "inc").await;
    }

    let up = n
        .client
        .upgrade(
            "ctr",
            bundle(
                &n.key,
                "ctr",
                "2.0.0",
                &counter(2),
                counter_manifest(Some("^1")),
                &[],
            ),
            &RolloutParams::default(),
        )
        .await
        .unwrap();
    let after_up = call(&n, "ctr", "get").await;
    let broken = n
        .client
        .upgrade(
            "ctr",
            bundle(
                &n.key,
                "ctr",
                "3.0.0",
                &counter(3),
                counter_manifest(Some("^2")),
                &[],
            ),
            &RolloutParams::default(),
        )
        .await
        .unwrap_err();
    let back = n
        .client
        .rollback("ctr", &RolloutParams::default())
        .await
        .unwrap();
    let after_back = call(&n, "ctr", "get").await;
    let list = n.client.list().await.unwrap();

    assert_eq!(
        (up.from.as_str(), up.to.as_str(), up.mode.as_str()),
        ("1.0.0", "2.0.0", "snapshot")
    );
    assert_eq!(after_up, "2.0.0 count=3 history=");
    assert_eq!(kind(&broken), "refused");
    assert!(
        broken.to_string().contains("v3 cannot read snapshots"),
        "{broken}"
    );
    assert_eq!((back.from.as_str(), back.to.as_str()), ("2.0.0", "1.0.0"));
    assert_eq!(after_back, "1.0.0 count=3");
    assert_eq!(
        (list[0].version.as_str(), list[0].previous.as_deref()),
        ("1.0.0", Some("2.0.0"))
    );
    assert!(!n.data().join("bundles/ctr/3.0.0.rbp").exists());
    assert_eq!(
        n.events(),
        ["deploy", "upgrade", "upgrade_refused", "rollback"]
    );
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart____node_comes_back____running_apps_restored_stopped_ones_not() {
    let mut n = TestNode::start("").await;
    n.client
        .deploy(bundle(
            &n.key,
            "keep",
            "1.0.0",
            &counter(1),
            counter_manifest(None),
            &[],
        ))
        .await
        .unwrap();
    n.client
        .upgrade(
            "keep",
            bundle(
                &n.key,
                "keep",
                "2.0.0",
                &counter(2),
                counter_manifest(Some("^1")),
                &[],
            ),
            &RolloutParams::default(),
        )
        .await
        .unwrap();
    n.client
        .deploy(bundle(
            &n.key,
            "parked",
            "1.0.0",
            &fixture("paddock_fixture_chaos"),
            fast(json!({})),
            &[],
        ))
        .await
        .unwrap();
    n.client.stop("parked").await.unwrap();

    n.restart().await;
    let list = n.client.list().await.unwrap();
    let get = call(&n, "keep", "get").await;
    let back = n
        .client
        .rollback("keep", &RolloutParams::default())
        .await
        .unwrap();

    let summary: Vec<(&str, &str, &str)> = list
        .iter()
        .map(|a| (a.name.as_str(), a.version.as_str(), a.status.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![("keep", "2.0.0", "running"), ("parked", "1.0.0", "stopped")]
    );
    assert!(get.starts_with("2.0.0 count=0"), "{get}");
    assert_eq!(back.to, "1.0.0");
    assert!(n.events().contains(&"restore".to_string()));
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_start_remove____lifecycle_and_dependency_guard() {
    let mut n = TestNode::start("").await;
    let chaos = fixture("paddock_fixture_chaos");
    n.client
        .deploy(bundle(
            &n.key,
            "base",
            "1.0.0",
            &chaos,
            fast(json!({})),
            &[],
        ))
        .await
        .unwrap();
    n.client
        .deploy(bundle(
            &n.key,
            "top",
            "1.0.0",
            &chaos,
            fast(json!({ "dependencies": [{ "app": "base", "version": "^1" }] })),
            &[],
        ))
        .await
        .unwrap();

    let blocked = n.client.stop("base").await.unwrap_err();
    n.client.stop("top").await.unwrap();
    let again = n.client.stop("top").await.unwrap_err();
    n.client.start("top").await.unwrap();
    let ps_after_start = n.client.ps().await.unwrap();
    let remove_blocked = n.client.remove("base").await.unwrap_err();
    n.client.remove("top").await.unwrap();
    n.client.remove("base").await.unwrap();

    assert_eq!(kind(&blocked), "conflict");
    assert_eq!(kind(&again), "conflict");
    assert!(ps_after_start.iter().any(|i| i.app == "top"));
    assert_eq!(kind(&remove_blocked), "conflict");
    assert!(n.client.list().await.unwrap().is_empty());
    assert!(!n.data().join("bundles/top").exists());
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors____unknown_app_pid_and_route____not_found_or_invalid() {
    let mut n = TestNode::start("").await;

    let app = n.client.status("ghost").await.unwrap_err();
    let pid = n.client.kill("not-a-pid").await.unwrap_err();
    let dead = n.client.kill("0.999.1").await.unwrap_err();
    let dup = {
        let b = bundle(
            &n.key,
            "once",
            "1.0.0",
            &fixture("paddock_fixture_chaos"),
            fast(json!({})),
            &[],
        );
        n.client.deploy(b.clone()).await.unwrap();
        n.client.deploy(b).await.unwrap_err()
    };

    assert_eq!(kind(&app), "not_found");
    assert_eq!(kind(&pid), "invalid");
    assert_eq!(kind(&dead), "not_found");
    assert_eq!(kind(&dup), "conflict");
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_socket____created_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mut n = TestNode::start("").await;

    let mode = std::fs::metadata(n.node().socket())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(mode, 0o600);
    n.shutdown().await;
}
