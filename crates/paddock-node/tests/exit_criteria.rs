//! Phase 1 exit criteria (docs/ROADMAP.md, "Status"), one test each, all through
//! a running node and its admin API. `paddock-host` tests cover the same
//! ground at the runtime level.

#![cfg(unix)]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{
    TestNode, bundle, call, counter, counter_manifest, fast, fixture, kind, services_manifest,
};
use minisign::KeyPair;
use paddock_api::RolloutParams;
use serde_json::json;

const CHAOS: &str = "paddock_fixture_chaos";

async fn deploy(n: &TestNode, name: &str, component: &[u8], manifest: serde_json::Value) {
    n.client
        .deploy(bundle(&n.key, name, "1.0.0", component, manifest, &[]))
        .await
        .unwrap();
}

/// The pid of `app`'s first active instance.
async fn pid_of(n: &TestNode, app: &str) -> String {
    next_pid(n, app, "").await
}

/// Wait for an active instance of `app` whose pid is not `old`.
async fn next_pid(n: &TestNode, app: &str, old: &str) -> String {
    for _ in 0..300 {
        let ps = n.client.ps().await.unwrap();
        if let Some(i) = ps
            .iter()
            .find(|i| i.app == app && i.pid != old && i.state == "active")
        {
            return i.pid.clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "no new instance of {app}: {:?}",
        n.client.ps().await.unwrap()
    );
}

/// Call `app` with `body` until the reply contains every one of `wanted`.
async fn call_until(n: &TestNode, app: &str, body: &str, wanted: &[&str]) -> String {
    let mut reply = String::new();
    for _ in 0..300 {
        reply = call(n, app, body).await;
        if wanted.iter().all(|w| reply.contains(w)) {
            return reply;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    reply
}

fn incarnation(pid: &str) -> u64 {
    pid.rsplit('.').next().unwrap().parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_1____kill____restarted_links_get_exit_monitors_get_down_stale_pid_noproc() {
    let mut n = TestNode::start("").await;
    let chaos = fixture(CHAOS);
    deploy(&n, "target", &chaos, fast(json!({}))).await;
    deploy(
        &n,
        "watcher",
        &chaos,
        fast(json!({ "lifecycle": { "start_args": "trap-exits" } })),
    )
    .await;
    let target = pid_of(&n, "target").await;
    let linked = call(&n, "watcher", &format!("link:{target}")).await;
    let mref = call(&n, "watcher", &format!("monitor:{target}")).await;

    n.client.kill(&target).await.unwrap();
    let next = next_pid(&n, "target", &target).await;
    let exit = format!("exit:{target}:killed");
    let down = format!("down:{mref}:{target}:killed");
    let infos = call_until(&n, "watcher", "infos", &[&exit, &down]).await;
    let stale_call = call(&n, "watcher", &format!("call:{target}:500:echo:x")).await;
    let stale_kill = n.client.kill(&target).await.unwrap_err();
    let target_status = n.client.status("target").await.unwrap();
    let watcher_status = n.client.status("watcher").await.unwrap();

    assert_eq!(linked, "ok");
    assert_eq!(incarnation(&next), incarnation(&target) + 1);
    assert!(infos.contains(&exit) && infos.contains(&down), "{infos}");
    assert_eq!(stale_call, "err:noproc");
    assert_eq!(kind(&stale_kill), "not_found");
    assert_eq!((target_status.restarts, watcher_status.restarts), (1, 0));
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_2____init_traps____app_failed_with_reason_in_status() {
    let mut n = TestNode::start("").await;
    let b = bundle(
        &n.key,
        "crashy",
        "1.0.0",
        &fixture(CHAOS),
        fast(json!({ "lifecycle": { "start_args": "trap", "init_timeout_ms": 500 } })),
        &[],
    );

    let deployed = n.client.deploy(b).await;
    let status = n.client.status("crashy").await.unwrap();

    let reason = status.summary.reason.clone().unwrap_or_default();
    assert_eq!(status.summary.status, "failed", "{deployed:?}");
    assert!(reason.contains("max restart intensity"), "{reason}");
    assert!(reason.contains("trap"), "{reason}");
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_3____handler_spins____killed_with_timeout_and_node_stays_responsive() {
    let mut n = TestNode::start("").await;
    let chaos = fixture(CHAOS);
    deploy(
        &n,
        "runaway",
        &chaos,
        fast(json!({ "limits": { "callback_deadline_ms": 300 } })),
    )
    .await;
    deploy(&n, "bystander", &chaos, fast(json!({}))).await;
    let before = pid_of(&n, "runaway").await;
    let started = Instant::now();

    let spin = tokio::spawn({
        let client = n.client.clone();
        async move {
            client
                .call("runaway", "text/plain", b"spin".to_vec(), 10_000)
                .await
        }
    });
    let mut worst = Duration::ZERO;
    for _ in 0..10 {
        let t = Instant::now();
        assert_eq!(call(&n, "bystander", "echo:x").await, "x");
        n.client.ps().await.unwrap();
        worst = worst.max(t.elapsed());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let err = spin.await.unwrap().unwrap_err();
    let elapsed = started.elapsed();
    next_pid(&n, "runaway", &before).await;
    let after = call(&n, "runaway", "echo:back").await;

    assert!(err.to_string().contains("timeout"), "{err}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert!(
        worst < Duration::from_millis(250),
        "bystander latency {worst:?}"
    );
    assert_eq!(after, "back");
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_4____allocates_past_memory_max____memory_limit_and_others_unaffected() {
    let mut n = TestNode::start("").await;
    let chaos = fixture(CHAOS);
    deploy(
        &n,
        "hog",
        &chaos,
        fast(json!({ "limits": { "memory_max": "16MiB" } })),
    )
    .await;
    deploy(&n, "bystander", &chaos, fast(json!({}))).await;
    let before = pid_of(&n, "hog").await;
    assert_eq!(call(&n, "bystander", "count").await, "1");

    let small = call(&n, "hog", "alloc:4").await;
    let big = n
        .client
        .call("hog", "text/plain", b"alloc:64".to_vec(), 5_000)
        .await
        .unwrap_err();
    next_pid(&n, "hog", &before).await;
    let bystander = call(&n, "bystander", "count").await;
    let status = n.client.status("bystander").await.unwrap();

    assert_eq!(small, "ok");
    assert!(big.to_string().contains("memory_limit"), "{big}");
    assert_eq!(bystander, "2", "bystander kept its state");
    assert_eq!(status.restarts, 0);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_5____mailbox_full____casts_dropped_and_counted_calls_fail_fast() {
    let mut n = TestNode::start("").await;
    deploy(
        &n,
        "busy",
        &fixture(CHAOS),
        fast(json!({ "limits": { "mailbox_max": 1, "callback_deadline_ms": 30_000 } })),
    )
    .await;
    let pid = pid_of(&n, "busy").await;
    assert_eq!(call(&n, "busy", "echo:up").await, "up");
    n.client
        .cast("busy", "text/plain", b"spin".to_vec())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    n.client
        .cast("busy", "text/plain", b"count".to_vec())
        .await
        .unwrap();
    let started = Instant::now();

    let cast = n
        .client
        .cast("busy", "text/plain", b"count".to_vec())
        .await
        .unwrap_err();
    let called = n
        .client
        .call("busy", "text/plain", b"echo:x".to_vec(), 5_000)
        .await
        .unwrap_err();
    let elapsed = started.elapsed();
    let ps = n.client.ps().await.unwrap();

    assert!(cast.to_string().contains("overloaded"), "{cast}");
    assert!(called.to_string().contains("overloaded"), "{called}");
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    let busy = ps.iter().find(|i| i.pid == pid).unwrap();
    assert_eq!((busy.queued, busy.dropped), (1, 1));
    n.client.kill(&pid).await.unwrap();
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_criterion_6____upgrade____state_pid_name_and_links_kept_broken_v3_reverts_losing_nothing()
 {
    let mut n = TestNode::start("").await;
    let mut v1 = counter_manifest(None);
    v1["lifecycle"]["start_args"] = json!("register:ctr");
    v1["registered_names"] = json!(["ctr"]);
    let mut v2 = counter_manifest(Some("^1"));
    v2["lifecycle"]["start_args"] = json!("register:ctr");
    v2["registered_names"] = json!(["ctr"]);
    let mut v3 = counter_manifest(Some("^2"));
    v3["lifecycle"]["start_args"] = json!("register:ctr");
    v3["registered_names"] = json!(["ctr"]);
    deploy(&n, "ctr", &counter(1), v1).await;
    deploy(
        &n,
        "watcher",
        &fixture(CHAOS),
        fast(json!({ "lifecycle": { "start_args": "trap-exits" } })),
    )
    .await;
    let pid = pid_of(&n, "ctr").await;
    assert_eq!(call(&n, "watcher", &format!("link:{pid}")).await, "ok");
    for _ in 0..3 {
        call(&n, "ctr", "inc").await;
    }

    n.client
        .upgrade(
            "ctr",
            bundle(&n.key, "ctr", "2.0.0", &counter(2), v2, &[]),
            &RolloutParams::default(),
        )
        .await
        .unwrap();
    let after_up = call(&n, "ctr", "get").await;
    let pid_after_up = pid_of(&n, "ctr").await;
    let named = call(&n, "watcher", "whereis:ctr").await;
    let state = n.client.state(&pid).await.unwrap();

    let incs: Vec<_> = (0..20)
        .map(|_| {
            let client = n.client.clone();
            tokio::spawn(async move {
                client
                    .call("ctr", "text/plain", b"inc".to_vec(), 10_000)
                    .await
            })
        })
        .collect();
    let broken = n
        .client
        .upgrade(
            "ctr",
            bundle(&n.key, "ctr", "3.0.0", &counter(3), v3, &[]),
            &RolloutParams::default(),
        )
        .await
        .unwrap_err();
    let mut lost = 0;
    for inc in incs {
        if inc.await.unwrap().is_err() {
            lost += 1;
        }
    }
    let after_revert = call(&n, "ctr", "get").await;
    n.client.kill(&pid).await.unwrap();
    let exit = format!("exit:{pid}:killed");
    let infos = call_until(&n, "watcher", "infos", &[&exit]).await;

    assert_eq!(after_up, "2.0.0 count=3 history=");
    assert_eq!(pid_after_up, pid);
    assert_eq!(named, pid);
    assert_eq!(state.format, "text");
    assert_eq!(state.value, Some(json!("3;")));
    assert_eq!(kind(&broken), "refused");
    assert!(
        broken.to_string().contains("v3 cannot read snapshots"),
        "{broken}"
    );
    assert_eq!(lost, 0);
    assert_eq!(after_revert, "2.0.0 count=23 history=21,22,23");
    assert!(infos.contains(&exit), "link survived the upgrades: {infos}");
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
async fn exit_criterion_8____disk_bucket_and_files____survive_crash_and_restart_priv_read_only_no_escape()
 {
    let scratch = tempfile::tempdir().unwrap();
    let policy_dir = tempfile::tempdir().unwrap();
    let policy = policy_dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[defaults]\ngrant = [\"wasi:logging\", \"wasi:config\", \"wasi:keyvalue\", \"wasi:filesystem\"]\n",
    )
    .unwrap();
    let mut n = TestNode::start(&format!(
        "policy = {:?}\n[volumes]\nscratch = {:?}\n",
        policy,
        scratch.path()
    ))
    .await;
    n.client
        .deploy(bundle(
            &n.key,
            "svc",
            "1.0.0",
            &fixture("paddock_fixture_services"),
            services_manifest(),
            &[("priv/motd.txt", b"from the bundle")],
        ))
        .await
        .unwrap();
    let before = pid_of(&n, "svc").await;
    assert_eq!(call(&n, "svc", "kv-set:data:k:kept").await, "ok");
    assert_eq!(call(&n, "svc", "write:/data/note.txt:written").await, "ok");

    n.client
        .call("svc", "text/plain", b"trap".to_vec(), 5_000)
        .await
        .unwrap_err();
    next_pid(&n, "svc", &before).await;
    let kv_after_crash = call(&n, "svc", "kv-get:data:k").await;
    let priv_read = call(&n, "svc", "read:/priv/motd.txt").await;
    let priv_write = call(&n, "svc", "write:/priv/motd.txt:changed").await;
    let escapes = [
        call(&n, "svc", "read:/data/../../../../etc/hostname").await,
        call(&n, "svc", "read:/priv/../../../../etc/hostname").await,
        call(&n, "svc", "write:/data/../escaped.txt:x").await,
    ];
    n.restart().await;
    let kv_after_restart = call(&n, "svc", "kv-get:data:k").await;
    let file_after_restart = call(&n, "svc", "read:/data/note.txt").await;

    assert_eq!(kv_after_crash, "kept");
    assert_eq!(priv_read, "from the bundle");
    assert!(priv_write.starts_with("err:"), "{priv_write}");
    for e in &escapes {
        assert!(e.starts_with("err:"), "{e}");
    }
    assert!(
        !scratch
            .path()
            .parent()
            .unwrap()
            .join("escaped.txt")
            .exists()
    );
    assert_eq!(kv_after_restart, "kept");
    assert_eq!(file_after_restart, "written");
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
