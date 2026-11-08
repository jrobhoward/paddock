//! A node end to end through its admin API: deploy, status, kill, upgrade,
//! rollback, stop/start/remove, restart restore, refusals, logs and the
//! audit log. The Phase 1 exit criteria are in `exit_criteria.rs`.

// Native apps run on Linux only.
#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use bulkhead::api::RolloutParams;
use common::{
    TestNode, app_bundle, call, counter_manifest, fast, kind, native_app, native_counter,
    native_net, native_sdk, services_manifest,
};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy____signed_bundle____running_listed_and_callable() {
    let mut n = TestNode::start_native("").await;
    let b = app_bundle(
        &n.key,
        "echoer",
        "1.0.0",
        &native_app(),
        fast(json!({ "instances": { "count": 2 } })),
    );

    let deployed = n.client.deploy(b).await.unwrap();
    let list = n.client.list().await.unwrap();
    let ps = n.client.ps().await.unwrap();
    let reply = call(&n, "echoer", "echo hello").await;

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
    let mut n = TestNode::start_native("").await;
    n.client
        .deploy(app_bundle(
            &n.key,
            "victim",
            "1.0.0",
            &native_app(),
            fast(json!({})),
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
        before
            .split(['.', '@'])
            .map(|x| x.parse().unwrap())
            .collect(),
        after
            .split(['.', '@'])
            .map(|x| x.parse().unwrap())
            .collect(),
    );
    assert_eq!(a[1], b[1]);
    assert_eq!(a[2], b[2] + 1);
    assert_eq!(status.restarts, 1);
    assert!(n.events().contains(&"kill".to_string()));
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_and_logs____node_and_app_both_set____node_values_win_and_records_are_kept() {
    let scratch = tempfile::tempdir().unwrap();
    let policy_dir = tempfile::tempdir().unwrap();
    let policy = policy_dir.path().join("policy.toml");
    std::fs::write(&policy, "[apps.svclog]\ngrant = [\"wasi:logging\", \"wasi:config\", \"wasi:keyvalue\", \"wasi:filesystem\"]\n").unwrap();
    let mut n = TestNode::start_native(&format!(
        "policy = {:?}\n[volumes]\nscratch = {:?}\n[config.svclog]\nmode = \"from-node\"\n",
        policy,
        scratch.path()
    ))
    .await;
    n.client
        .deploy(app_bundle(
            &n.key,
            "svclog",
            "1.0.0",
            &native_app(),
            services_manifest(),
        ))
        .await
        .unwrap();

    let all = call(&n, "svclog", "config-all").await;
    call(&n, "svclog", "log-at warn disk almost full").await;
    call(&n, "svclog", "print to stdout").await;
    let mut lines = Vec::new();
    for _ in 0..100 {
        lines = n.client.logs("svclog", 10).await.unwrap();
        if lines.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(all, "greeting=hello,mode=from-node");
    let messages: Vec<(&str, &str)> = lines
        .iter()
        .map(|l| (l.source.as_str(), l.message.as_str()))
        .collect();
    assert_eq!(
        messages,
        // A native record has no context, and its streams share one pipe.
        vec![("", "disk almost full"), ("stderr", "to stdout")]
    );
    assert_eq!(lines[0].level, "WARN");

    n.client
        .set_log_level("svclog", Some("error"))
        .await
        .unwrap();
    call(&n, "svclog", "log-at warn hidden while capped").await;
    n.client.set_log_level("svclog", None).await.unwrap();
    call(&n, "svclog", "log-at warn shown after reset").await;
    let mut after = Vec::new();
    for _ in 0..100 {
        after = n.client.logs("svclog", 10).await.unwrap();
        if after.iter().any(|l| l.message == "shown after reset") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let unknown = n
        .client
        .set_log_level("nobody", Some("debug"))
        .await
        .unwrap_err();

    assert!(!after.iter().any(|l| l.message == "hidden while capped"));
    assert!(after.iter().any(|l| l.message == "shown after reset"));
    assert_eq!(kind(&unknown), "not_found");
    assert_eq!(n.events().iter().filter(|e| *e == "log_level").count(), 2);
    n.shutdown().await;
}

/// The next event of `kind` and `event` on `events`, within 10 seconds.
async fn next_event(
    events: &mut bulkhead::api::Follow<bulkhead::api::Event>,
    kind: &str,
    event: &str,
) -> bulkhead::api::Event {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let e = events.next().await.unwrap().unwrap();
            if e.kind == kind && e.event == event {
                return e;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind} {event} event"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_and_follow____deploy_kill_and_logs____streamed_as_they_happen() {
    let scratch = tempfile::tempdir().unwrap();
    let policy_dir = tempfile::tempdir().unwrap();
    let policy = policy_dir.path().join("policy.toml");
    std::fs::write(&policy, "[apps.svcfollow]\ngrant = [\"wasi:logging\", \"wasi:config\", \"wasi:keyvalue\", \"wasi:filesystem\"]\n").unwrap();
    let mut n = TestNode::start_native(&format!(
        "policy = {:?}\n[volumes]\nscratch = {:?}\n",
        policy,
        scratch.path()
    ))
    .await;
    let mut events = n.client.events().await.unwrap();

    n.client
        .deploy(app_bundle(
            &n.key,
            "svcfollow",
            "1.0.0",
            &native_app(),
            services_manifest(),
        ))
        .await
        .unwrap();
    let deployed = next_event(&mut events, "audit", "deploy").await;
    call(&n, "svcfollow", "log-at warn before the follow").await;
    let mut logs = n.client.follow_logs("svcfollow", 1).await.unwrap();
    let backlog = logs.next().await.unwrap().unwrap();
    call(&n, "svcfollow", "log-at warn while following").await;
    let live = tokio::time::timeout(Duration::from_secs(10), logs.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let before = n.client.ps().await.unwrap()[0].pid.clone();
    n.client.kill(&before).await.unwrap();
    let killed = next_event(&mut events, "audit", "kill").await;
    let exited = next_event(&mut events, "instance", "exit").await;
    let restarted = next_event(&mut events, "instance", "restart").await;
    let unknown = n.client.follow_logs("nobody", 1).await.unwrap_err();
    n.shutdown().await;
    // Records logged while the app stopped may come first.
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while logs.next().await?.is_some() {}
        Ok::<_, bulkhead::api::ClientError>(())
    })
    .await;

    assert_eq!(deployed.fields["app"], "svcfollow");
    assert_eq!(backlog.message, "before the follow");
    assert_eq!(live.message, "while following");
    assert_eq!(killed.fields["pid"], format!("<{before}>"));
    assert_eq!(exited.fields["pid"], before.as_str());
    assert_eq!(exited.fields["app"], "svcfollow");
    assert!(
        exited.fields["reason"].as_str().unwrap().contains("killed"),
        "{exited:?}"
    );
    assert_eq!(restarted.fields["previous"], before.as_str());
    assert_ne!(restarted.fields["pid"], before.as_str());
    assert_eq!(kind(&unknown), "not_found");
    assert!(
        matches!(ended, Ok(Ok(()))),
        "the stream ends with the node: {ended:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade_and_rollback____through_the_api____state_kept_and_audited() {
    let mut n = TestNode::start_native("").await;
    n.client
        .deploy(app_bundle(
            &n.key,
            "ctr",
            "1.0.0",
            &native_counter(1),
            counter_manifest(None),
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
            app_bundle(
                &n.key,
                "ctr",
                "2.0.0",
                &native_counter(2),
                counter_manifest(Some("^1")),
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
            app_bundle(
                &n.key,
                "ctr",
                "3.0.0",
                &native_counter(3),
                counter_manifest(Some("^2")),
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
async fn rollback____previous_cannot_read_snapshot____refused_then_restart_discards_state() {
    // v3's load-snapshot always fails, so a snapshot rollback from v2 to v3
    // is refused.
    let mut n = TestNode::start_native("").await;
    n.client
        .deploy(app_bundle(
            &n.key,
            "rb",
            "3.0.0",
            &native_counter(3),
            counter_manifest(None),
        ))
        .await
        .unwrap();
    n.client
        .upgrade(
            "rb",
            app_bundle(
                &n.key,
                "rb",
                "2.0.0",
                &native_counter(2),
                counter_manifest(Some("^3")),
            ),
            &RolloutParams::default(),
        )
        .await
        .unwrap();
    for _ in 0..2 {
        call(&n, "rb", "inc").await;
    }

    let refused = n
        .client
        .rollback("rb", &RolloutParams::default())
        .await
        .unwrap_err();
    let still = call(&n, "rb", "get").await;
    let back = n
        .client
        .rollback(
            "rb",
            &RolloutParams {
                restart: true,
                ..RolloutParams::default()
            },
        )
        .await
        .unwrap();
    let after = call(&n, "rb", "get").await;
    let list = n.client.list().await.unwrap();

    assert_eq!(kind(&refused), "refused");
    assert!(refused.to_string().contains("--restart"), "{refused}");
    assert_eq!(still, "2.0.0 count=2 history=1,2");
    assert_eq!(
        (back.from.as_str(), back.to.as_str(), back.mode.as_str()),
        ("2.0.0", "3.0.0", "restart")
    );
    assert_eq!(after, "3.0.0 count=0 history=");
    assert_eq!(
        (list[0].version.as_str(), list[0].previous.as_deref()),
        ("3.0.0", Some("2.0.0"))
    );
    let audit = n.audit();
    let last = audit.last().unwrap();
    assert_eq!(
        (&last["event"], &last["mode"], &last["restart_requested"]),
        (&json!("rollback"), &json!("restart"), &json!(true))
    );
    assert_eq!(
        n.events(),
        ["deploy", "upgrade", "rollback_refused", "rollback"]
    );
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart____node_comes_back____running_apps_restored_stopped_ones_not() {
    let mut n = TestNode::start_native("").await;
    n.client
        .deploy(app_bundle(
            &n.key,
            "keep",
            "1.0.0",
            &native_counter(1),
            counter_manifest(None),
        ))
        .await
        .unwrap();
    n.client
        .upgrade(
            "keep",
            app_bundle(
                &n.key,
                "keep",
                "2.0.0",
                &native_counter(2),
                counter_manifest(Some("^1")),
            ),
            &RolloutParams::default(),
        )
        .await
        .unwrap();
    n.client
        .deploy(app_bundle(
            &n.key,
            "parked",
            "1.0.0",
            &native_app(),
            fast(json!({})),
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
async fn stop_start_remove____app_with_dependents____lifecycle_and_dependency_guard_hold() {
    let mut n = TestNode::start_native("").await;
    let chaos = native_app();
    n.client
        .deploy(app_bundle(&n.key, "base", "1.0.0", &chaos, fast(json!({}))))
        .await
        .unwrap();
    n.client
        .deploy(app_bundle(
            &n.key,
            "top",
            "1.0.0",
            &chaos,
            fast(json!({ "dependencies": [{ "app": "base", "version": "^1" }] })),
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
    let mut n = TestNode::start_native("").await;

    let app = n.client.status("ghost").await.unwrap_err();
    let pid = n.client.kill("not-a-pid").await.unwrap_err();
    let dead = n.client.kill("0.999.1").await.unwrap_err();
    let dup = {
        let b = app_bundle(&n.key, "once", "1.0.0", &native_app(), fast(json!({})));
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
async fn admin_socket____node_starts____created_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mut n = TestNode::start_native("").await;

    let mode = std::fs::metadata(n.node().socket())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(mode, 0o600);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open____data_dir_held_by_another_node____refused() {
    let mut n = TestNode::start_native("").await;
    let toml = format!("data_dir = {:?}\n", n.data());

    let second = bulkhead::node::Node::open(
        bulkhead::node::NodeConfig::from_toml_str(&toml).unwrap(),
        common::ring(),
    )
    .await;

    let Err(err) = second else {
        panic!("a second node opened the same data directory");
    };
    assert!(
        matches!(err, bulkhead::node::NodeError::Conflict(_)),
        "{err}"
    );
    assert!(err.to_string().contains("in use by another node"), "{err}");
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node____witness_role____deploy_refused_and_apps_on_disk_refuse_the_role() {
    let mut w = TestNode::start_native("[cluster]\nrole = \"witness\"\n").await;
    let refused = w
        .client
        .deploy(app_bundle(
            &w.key,
            "echoer",
            "1.0.0",
            &native_app(),
            fast(json!({})),
        ))
        .await
        .unwrap_err();
    w.shutdown().await;
    let mut m = TestNode::start_native("").await;
    m.client
        .deploy(app_bundle(
            &m.key,
            "echoer",
            "1.0.0",
            &native_app(),
            fast(json!({})),
        ))
        .await
        .unwrap();
    m.shutdown().await;
    m.release();

    let toml = format!("data_dir = {:?}\n[cluster]\nrole = \"witness\"\n", m.data());
    let mut reopened = None;
    for _ in 0..250 {
        match bulkhead::node::Node::open(
            bulkhead::node::NodeConfig::from_toml_str(&toml).unwrap(),
            common::ring(),
        )
        .await
        {
            Err(bulkhead::node::NodeError::Conflict(_)) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            other => {
                reopened = Some(other);
                break;
            }
        }
    }

    assert_eq!(kind(&refused), "refused");
    assert!(refused.to_string().contains("witness"), "{refused}");
    let Some(Err(err)) = reopened else {
        panic!("a witness opened a data directory holding apps");
    };
    assert!(err.to_string().contains("witness"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node____restarted____creation_raised_on_every_start() {
    let mut n = TestNode::start_native("").await;
    let first = std::fs::read_to_string(n.data().join("creation")).unwrap();

    n.restart().await;
    let second = std::fs::read_to_string(n.data().join("creation")).unwrap();

    assert_eq!((first.as_str(), second.as_str()), ("1", "2"));
    n.shutdown().await;
}

/// The services fixture's manifest with these shared buckets listed.
fn shared_manifest(shared: serde_json::Value) -> serde_json::Value {
    let mut m = services_manifest();
    m["capabilities"]["wasi:keyvalue"]["shared"] = shared;
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_bucket____two_apps_through_policy____shared_read_only_and_kept_on_restart() {
    let scratch = tempfile::tempdir().unwrap();
    let policy_dir = tempfile::tempdir().unwrap();
    let policy = policy_dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[defaults]\ngrant = [\"wasi:keyvalue\"]\nbuckets = [\"board\"]\n",
    )
    .unwrap();
    let mut n = TestNode::start_native(&format!(
        "policy = {policy:?}\n[volumes]\nscratch = {:?}\n\
         [buckets.board]\ndurability = \"disk\"\n[buckets.alarms]\n",
        scratch.path()
    ))
    .await;
    let services = native_app();
    let deploy =
        |name: &str, shared| app_bundle(&n.key, name, "1.0.0", &services, shared_manifest(shared));
    n.client
        .deploy(deploy("plc", json!({ "board": "rw" })))
        .await
        .unwrap();
    let hmi = n
        .client
        .deploy(deploy("hmi", json!({ "board": "ro", "alarms": "ro" })))
        .await
        .unwrap();
    let refused = n
        .client
        .deploy(deploy("lost", json!({ "nowhere": "ro" })))
        .await
        .unwrap_err();

    call(&n, "plc", "kv set board pump-1 on").await;
    let seen = call(&n, "hmi", "kv get board pump-1").await;
    let write = call(&n, "hmi", "kv set board pump-1 off").await;
    let ungranted = call(&n, "hmi", "kv get alarms a1").await;
    n.restart().await;
    let after_restart = call(&n, "hmi", "kv get board pump-1").await;

    assert!(hmi.granted.contains(&"bucket:board".to_string()), "{hmi:?}");
    assert!(hmi.denied.contains(&"bucket:alarms".to_string()), "{hmi:?}");
    assert!(!hmi.denied.contains(&"bucket:board".to_string()), "{hmi:?}");
    assert_eq!(kind(&refused), "invalid");
    assert_eq!(seen, "on");
    assert_eq!(write, "err:AccessDenied");
    assert_eq!(ungranted, "err:AccessDenied");
    assert_eq!(after_restart, "on");
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets____granted_by_name____read_and_kept_out_of_the_audit_log() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("db");
    std::fs::write(&secret, "hunter2").unwrap();
    let policy = dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.vault]\ngrant = [\"bulkhead:secrets\"]\nsecrets = [\"db\"]\n",
    )
    .unwrap();
    let mut n = TestNode::start_native(&format!(
        "policy = {policy:?}\n[secrets.db]\nfile = {secret:?}\n[secrets.other]\nenv = \"BULKHEAD_UNSET_FOR_TEST\"\n"
    ))
    .await;
    let manifest = |names: serde_json::Value| {
        fast(json!({ "capabilities": {
            "wasi:config": {},
            "wasi:keyvalue": {},
            "bulkhead:groups": {},
            "bulkhead:metrics": { "prefix": "vault" },
            "bulkhead:secrets": { "secrets": names },
            // The SDK fixture imports `bulkhead:sql` and `wasi:http`;
            // declared, nothing used.
            "bulkhead:sql": {},
            "wasi:http/outgoing-handler": {}
        } }))
    };
    let sdk = native_sdk();

    let unbound = n
        .client
        .deploy(app_bundle(
            &n.key,
            "vault",
            "1.0.0",
            &sdk,
            manifest(json!(["db", "nowhere"])),
        ))
        .await;
    let deployed = n
        .client
        .deploy(app_bundle(
            &n.key,
            "vault",
            "1.0.0",
            &sdk,
            manifest(json!(["db", "other"])),
        ))
        .await
        .unwrap();
    let value = call(&n, "vault", "secret:db").await;
    let denied = call(&n, "vault", "secret:other").await;

    assert_eq!(kind(&unbound.unwrap_err()), "invalid");
    assert!(
        deployed.granted.contains(&"secret:db".to_string()),
        "{deployed:?}"
    );
    assert!(
        deployed.denied.contains(&"secret:other".to_string()),
        "{deployed:?}"
    );
    assert_eq!(value, "hunter2");
    assert_eq!(denied, "err:access denied");
    let audit = serde_json::to_string(&n.audit()).unwrap();
    assert!(!audit.contains("hunter2"), "{audit}");
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics____app_and_host____on_the_admin_api_and_the_scrape_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let policy = dir.path().join("policy.toml");
    std::fs::write(&policy, "[apps.shop]\ngrant = [\"bulkhead:metrics\"]\n").unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut n = TestNode::start_native(&format!(
        "policy = {policy:?}\n[metrics]\naddress = \"127.0.0.1:{port}\"\n"
    ))
    .await;
    let manifest = fast(json!({ "capabilities": {
        "wasi:config": {},
        "wasi:keyvalue": {},
        "bulkhead:groups": {},
        "bulkhead:secrets": { "secrets": [] },
        "bulkhead:sql": {},
        "wasi:http/outgoing-handler": {},
        "bulkhead:metrics": { "prefix": "shop" }
    } }));
    n.client
        .deploy(app_bundle(&n.key, "shop", "1.0.0", &native_sdk(), manifest))
        .await
        .unwrap();

    let counted = call(&n, "shop", "counter:orders_total:eu:4").await;
    let admin = n.client.metrics().await.unwrap();
    let scraped = {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        s.write_all(b"GET /metrics HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).await.unwrap();
        raw
    };

    assert_eq!(counted, "ok");
    for text in [&admin, &scraped] {
        assert!(
            text.contains(r#"shop_orders_total{app="shop",l="eu"} 4"#),
            "{text}"
        );
        assert!(
            text.contains(r#"bulkhead_instances{app="shop"} 1"#),
            "{text}"
        );
    }
    assert!(scraped.starts_with("HTTP/1.1 200"), "{scraped}");
    assert!(scraped.contains("text/plain; version=0.0.4"), "{scraped}");
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_names____listed_by_another_app____deploy_and_upgrade_refused() {
    let n = TestNode::start_native("").await;
    let chaos = native_app();
    let named = |app: &str, version: &str, names: &[&str]| {
        app_bundle(
            &n.key,
            app,
            version,
            &chaos,
            fast(json!({ "registered_names": names })),
        )
    };
    n.client
        .deploy(named("first", "1.0.0", &["orders"]))
        .await
        .unwrap();

    let taken = n
        .client
        .deploy(named("second", "1.0.0", &["orders"]))
        .await
        .unwrap_err();
    let other = n
        .client
        .deploy(named("second", "1.0.0", &["billing"]))
        .await;
    let upgrade = n
        .client
        .upgrade(
            "second",
            named("second", "2.0.0", &["billing", "orders"]),
            &RolloutParams::default(),
        )
        .await
        .unwrap_err();
    let own = n
        .client
        .upgrade(
            "first",
            named("first", "2.0.0", &["orders"]),
            &RolloutParams::default(),
        )
        .await;

    assert_eq!(kind(&taken), "conflict");
    assert!(
        taken
            .to_string()
            .contains("'orders' is already listed by first"),
        "{taken}"
    );
    assert!(other.is_ok(), "{other:?}");
    assert_eq!(kind(&upgrade), "conflict");
    assert!(own.is_ok(), "{own:?}");
}

/// Instances that passivate go passive, and a call wakes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy____passivating_instances____go_passive_and_a_call_wakes_one() {
    let mut n = TestNode::start_native("max_processes = 64\n").await;
    n.client
        .deploy(app_bundle(
            &n.key,
            "sleepy",
            "1.0.0",
            &native_counter(1),
            fast(json!({ "instances": { "count": 5, "passivate_after_ms": 50 } })),
        ))
        .await
        .unwrap();
    for _ in 0..5 {
        call(&n, "sleepy", "inc").await;
    }
    let mut passive = 0;
    for _ in 0..100 {
        let ps = n.client.ps().await.unwrap();
        passive = ps
            .iter()
            .filter(|i| i.app == "sleepy" && i.state == "passive")
            .count();
        if passive == 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let woken = call(&n, "sleepy", "get").await;

    assert_eq!(passive, 5, "every sleepy instance passivated");
    assert!(woken.starts_with("1.0.0 count="), "{woken}");
    n.shutdown().await;
}

/// A server answering every request on one connection with `200 hi`.
async fn hello_server() -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut buf = [0u8; 1024];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    while let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                        seen.drain(..end + 4);
                        let reply = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhi";
                        if stream.write_all(reply).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    address
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress____inside_and_outside_the_node_ceiling____fetches_or_refused() {
    let address = hello_server().await;
    let policy_dir = tempfile::tempdir().unwrap();
    let policy = policy_dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.fetcher]\ngrant = [\"wasi:http/outgoing-handler\"]\n",
    )
    .unwrap();
    let mut n = TestNode::start_native(&format!(
        "policy = {policy:?}\n[egress]\nallow = [\"http://{address}\", \"*.internal\"]\n"
    ))
    .await;
    let manifest = |allow: serde_json::Value| {
        fast(json!({ "capabilities": {
            "wasi:config": {},
            "wasi:keyvalue": {},
            "bulkhead:groups": {},
            "bulkhead:secrets": { "secrets": [] },
            "bulkhead:sql": {},
            "bulkhead:metrics": { "prefix": "fetcher" },
            "wasi:http/outgoing-handler": { "allow": allow }
        } }))
    };
    let sdk = native_sdk();

    let outside = n
        .client
        .deploy(app_bundle(
            &n.key,
            "fetcher",
            "1.0.0",
            &sdk,
            manifest(json!(["https://api.example.com"])),
        ))
        .await
        .unwrap_err();
    n.client
        .deploy(app_bundle(
            &n.key,
            "fetcher",
            "1.0.0",
            &sdk,
            manifest(json!([format!("http://{address}"), "*.plant.internal"])),
        ))
        .await
        .unwrap();
    let fetched = call(&n, "fetcher", &format!("fetch GET http://{address}/x")).await;
    let other = call(&n, "fetcher", "fetch GET https://api.example.com/").await;

    assert_eq!(kind(&outside), "invalid");
    assert!(
        outside
            .to_string()
            .contains("\"https://api.example.com\" is outside"),
        "{outside}"
    );
    assert_eq!(fetched, "200 hi");
    assert_eq!(other, "err:denied");
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sockets____inside_and_outside_the_node_ceiling____connects_or_refused() {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream.write_all(b"hello").await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    let policy_dir = tempfile::tempdir().unwrap();
    let policy = policy_dir.path().join("policy.toml");
    std::fs::write(&policy, "[apps.mqtt]\ngrant = [\"wasi:sockets\"]\n").unwrap();
    let mut n = TestNode::start_native(&format!(
        "policy = {policy:?}\n[sockets]\nconnect = [{{ cidr = \"127.0.0.1/32\", ports = \"{}\" }}]\n",
        address.port()
    ))
    .await;
    let manifest =
        |scope: serde_json::Value| fast(json!({ "capabilities": { "wasi:sockets": scope } }));
    let net = native_net();

    let outside = n
        .client
        .deploy(app_bundle(
            &n.key,
            "mqtt",
            "1.0.0",
            &net,
            manifest(json!({ "connect": [{ "cidr": "127.0.0.0/8" }] })),
        ))
        .await
        .unwrap_err();
    let listen = n
        .client
        .deploy(app_bundle(
            &n.key,
            "mqtt",
            "1.0.0",
            &net,
            manifest(json!({ "listen": true })),
        ))
        .await
        .unwrap_err();
    n.client
        .deploy(app_bundle(&n.key, "mqtt", "1.0.0", &net, manifest(json!({ "connect": [{ "cidr": "127.0.0.1/32", "ports": address.port().to_string() }] }))))
        .await
        .unwrap();
    let id = call(&n, "mqtt", &format!("connect {address}")).await;
    call(&n, "mqtt", &format!("watch {id}")).await;
    let mut received = String::new();
    for _ in 0..100 {
        received = call(&n, "mqtt", "received").await;
        if !received.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(kind(&outside), "invalid");
    assert!(
        outside
            .to_string()
            .contains("outside the node's [sockets] connect"),
        "{outside}"
    );
    assert!(listen.to_string().contains("listen"), "{listen}");
    assert_eq!(received, "1:hello");
    n.shutdown().await;
}

/// The counter in `blue-green` upgrade mode, two instances.
fn blue_green_counter(n: &TestNode, version: &str, build: u8) -> Vec<u8> {
    let mut manifest = fast(json!({ "instances": { "count": 2 } }));
    manifest["upgrade"] = json!({ "mode": "blue-green" });
    app_bundle(&n.key, "bg", version, &native_counter(build), manifest)
}

/// Of `n` calls to the app by name, how many reached 1.0.0 and 2.0.0.
async fn versions_reached(node: &TestNode, n: usize) -> (usize, usize) {
    let (mut one, mut two) = (0, 0);
    for _ in 0..n {
        let reply = call(node, "bg", "get").await;
        one += usize::from(reply.starts_with("1.0.0"));
        two += usize::from(reply.starts_with("2.0.0"));
    }
    (one, two)
}

/// A blue-green upgrade through the node: started at no
/// traffic (`steps=0`), moved by hand, kept across a restart at the same
/// weight, then promoted; a second one dropped by rollback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blue_green____start_move_restart_promote_and_drop____traffic_follows_and_is_kept() {
    let mut n = TestNode::start_native("").await;
    n.client
        .deploy(blue_green_counter(&n, "1.0.0", 1))
        .await
        .unwrap();
    let only_start = RolloutParams {
        steps: Some(vec![0]),
        ..RolloutParams::default()
    };

    let started = n
        .client
        .upgrade("bg", blue_green_counter(&n, "2.0.0", 2), &only_start)
        .await
        .unwrap();
    let at_zero = versions_reached(&n, 10).await;
    n.client
        .traffic("bg", Some(50), false, false, &RolloutParams::default())
        .await
        .unwrap();
    let at_half = versions_reached(&n, 10).await;
    n.restart().await;
    let restored = n.client.status("bg").await.unwrap().summary;
    let after_restart = versions_reached(&n, 10).await;
    let promoted = n
        .client
        .traffic("bg", None, true, false, &RolloutParams::default())
        .await
        .unwrap();
    let after_promote = versions_reached(&n, 10).await;
    let promoted_status = n.client.status("bg").await.unwrap().summary;
    n.client
        .upgrade("bg", blue_green_counter(&n, "3.0.0", 2), &only_start)
        .await
        .unwrap();
    let dropped = n
        .client
        .rollback("bg", &RolloutParams::default())
        .await
        .unwrap();
    let after_drop = n.client.status("bg").await.unwrap().summary;

    assert_eq!(
        (started.mode.as_str(), started.traffic),
        ("blue-green", Some(0))
    );
    assert_eq!(at_zero, (10, 0));
    assert_eq!(at_half, (5, 5));
    assert_eq!(
        (restored.new_version.as_deref(), restored.new_traffic),
        (Some("2.0.0"), Some(50))
    );
    assert_eq!(after_restart, (5, 5));
    assert_eq!(
        (promoted.from.as_str(), promoted.to.as_str()),
        ("1.0.0", "2.0.0")
    );
    assert_eq!(after_promote, (0, 10));
    assert_eq!(
        (
            promoted_status.version.as_str(),
            promoted_status.previous.as_deref(),
            promoted_status.new_version
        ),
        ("2.0.0", Some("1.0.0"), None)
    );
    assert_eq!(
        (dropped.from.as_str(), dropped.to.as_str()),
        ("3.0.0", "2.0.0")
    );
    assert_eq!(
        (after_drop.version.as_str(), after_drop.new_version),
        ("2.0.0", None)
    );
    let bundles = n.dir.path().join("data/bundles/bg");
    assert!(bundles.join("2.0.0.rbp").exists() && bundles.join("1.0.0.rbp").exists());
    assert!(!bundles.join("3.0.0.rbp").exists());
}

/// `steps` that end at 100 move the traffic and promote the new version in
/// one upgrade.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blue_green____steps_to_full____promoted_in_one_upgrade() {
    let n = TestNode::start_native("").await;
    n.client
        .deploy(blue_green_counter(&n, "1.0.0", 1))
        .await
        .unwrap();

    let up = n
        .client
        .upgrade(
            "bg",
            blue_green_counter(&n, "2.0.0", 2),
            &RolloutParams {
                steps: Some(vec![10, 50, 100]),
                observe_ms: Some(50),
                ..RolloutParams::default()
            },
        )
        .await
        .unwrap();
    let after = versions_reached(&n, 10).await;

    assert_eq!(
        (up.from.as_str(), up.to.as_str(), up.traffic),
        ("1.0.0", "2.0.0", None)
    );
    assert_eq!(after, (0, 10));
}

/// A new version that gives up during a step's watch is dropped: the
/// upgrade is refused, the old version serves alone, and nothing of the new
/// one is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blue_green____new_version_gives_up_mid_step____dropped_and_refused() {
    let n = TestNode::start_native("").await;
    n.client
        .deploy(blue_green_counter(&n, "1.0.0", 1))
        .await
        .unwrap();
    let mut fragile =
        fast(json!({ "instances": { "count": 2 }, "supervision": { "intensity": 0 } }));
    fragile["upgrade"] = json!({ "mode": "blue-green" });
    let v2 = app_bundle(&n.key, "bg", "2.0.0", &native_counter(2), fragile);
    let client = n.client.clone();
    let upgrade = tokio::spawn(async move {
        client
            .upgrade(
                "bg",
                v2,
                &RolloutParams {
                    steps: Some(vec![50, 100]),
                    observe_ms: Some(1_500),
                    ..RolloutParams::default()
                },
            )
            .await
    });
    let mut victim = None;
    for _ in 0..200 {
        let detail = n.client.status("bg").await.unwrap();
        victim = detail
            .instance_list
            .iter()
            .find(|i| i.version == "2.0.0")
            .map(|i| i.pid.clone());
        if victim.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    n.client.kill(&victim.unwrap()).await.unwrap();
    let refused = upgrade.await.unwrap().unwrap_err();
    let after = versions_reached(&n, 10).await;
    let status = n.client.status("bg").await.unwrap().summary;

    assert_eq!(kind(&refused), "refused", "{refused}");
    assert!(refused.to_string().contains("gave up at 50%"), "{refused}");
    assert_eq!(after, (10, 0));
    assert_eq!(
        (status.version.as_str(), status.new_version),
        ("1.0.0", None)
    );
    assert!(!n.dir.path().join("data/bundles/bg/2.0.0.rbp").exists());
}

// Native apps run on Linux only (`bulkhead-runner`).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn native____bundle_for_both_platforms____runs_this_platforms_program() {
    use common::{native_bundle_for, native_fixtures, other_platform_elf};
    use rustbridge_bundle::Platform;

    let (runner, program) = native_fixtures();
    let program = std::fs::read(program).unwrap();
    let (other, other_program) = other_platform_elf();
    let this = Platform::current().unwrap();
    let n = TestNode::start(&format!("[native]\nrunner = {runner:?}\n")).await;
    let manifest = fast(json!({ "kind": "native-process", "native": { "program": "executable" } }));

    n.client
        .deploy(native_bundle_for(
            &n.key,
            "vendor",
            "1.0.0",
            &[(this, &program), (other, &other_program)],
            manifest.clone(),
        ))
        .await
        .unwrap();
    let echo = call(&n, "vendor", "echo both").await;
    let missing = n
        .client
        .deploy(native_bundle_for(
            &n.key,
            "vendor2",
            "1.0.0",
            &[(other, &other_program)],
            manifest,
        ))
        .await
        .unwrap_err();

    assert_eq!(echo, "both");
    assert!(
        missing.to_string().contains(&format!(
            "no program for {}, this node's platform (it has {})",
            this.as_str(),
            other.as_str()
        )),
        "{missing}"
    );
}

// Native apps run on Linux only (`bulkhead-runner`).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn native____filesystem_declared_and_denied_by_policy____no_volume() {
    use common::{native_bundle, native_fixtures};

    let (runner, program) = native_fixtures();
    let program = std::fs::read(program).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let volume = dir.path().join("volume");
    std::fs::create_dir(&volume).unwrap();
    std::fs::write(volume.join("f"), "in the volume").unwrap();
    let policy = dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.allowed]\ngrant = [\"wasi:filesystem\"]\n[apps.refused]\ngrant = []\n",
    )
    .unwrap();
    let n = TestNode::start(&format!(
        "policy = {policy:?}\n[native]\nrunner = {runner:?}\n[volumes]\nscratch = {volume:?}\n"
    ))
    .await;
    let manifest = fast(json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "capabilities": {
            "wasi:filesystem": { "preopens": { "/data": { "volume": "scratch", "mode": "ro" } } }
        }
    }));
    for app in ["allowed", "refused"] {
        n.client
            .deploy(native_bundle(
                &n.key,
                app,
                "1.0.0",
                &program,
                manifest.clone(),
            ))
            .await
            .unwrap();
    }

    let allowed = call(&n, "allowed", "vcat /data/f").await;
    let refused = call(&n, "refused", "vcat /data/f").await;

    assert_eq!(allowed, "in the volume");
    assert!(refused.starts_with("err:"), "{refused}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native____executable_deployed____answers_and_network_refused() {
    use common::{native_bundle, native_fixtures};

    let (runner, program) = native_fixtures();
    let program = std::fs::read(program).unwrap();
    let n = TestNode::start(&format!("[native]\nrunner = {runner:?}\n")).await;
    let manifest = |extra: serde_json::Value| {
        let mut m = json!({ "kind": "native-process", "native": { "program": "executable" } });
        if let (Some(m), Some(e)) = (m.as_object_mut(), extra.as_object()) {
            m.extend(e.clone());
        }
        fast(m)
    };

    n.client
        .deploy(native_bundle(
            &n.key,
            "vendor",
            "1.0.0",
            &program,
            manifest(json!({})),
        ))
        .await
        .unwrap();
    let echo = call(&n, "vendor", "echo native").await;
    let read = call(&n, "vendor", "read /etc/hostname").await;
    let refused = n
        .client
        .deploy(native_bundle(
            &n.key,
            "vendor2",
            "1.0.0",
            &program,
            manifest(json!({ "capabilities": { "bulkhead:runtime/supervisor": {} } })),
        ))
        .await
        .unwrap_err();

    assert_eq!(echo, "native");
    assert!(read.contains("Permission denied"), "{read}");
    assert!(
        format!("{refused:?}").contains("a native-process app may declare only"),
        "{refused:?}"
    );
}
