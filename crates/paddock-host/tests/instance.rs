//! Instances end to end, against the chaos fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{LONG, ask, body, shared, spec, text};
use paddock_bundle::manifest::{Capability, LogLevel};
use paddock_host::{CallError, ExitReason, LifecycleState};
use rstest::rstest;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____echo____replies() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("")).unwrap();

    let reply = instance.call(text("echo:hello"), LONG).await.unwrap();

    assert_eq!(body(&reply), "hello");
    assert_eq!(instance.state(), LifecycleState::Active);
    instance.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____count_repeatedly____state_kept_between_callbacks() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("")).unwrap();

    let mut counts = Vec::new();
    for _ in 0..3 {
        counts.push(body(&instance.call(text("count"), LONG).await.unwrap()));
    }

    assert_eq!(counts, vec!["1", "2", "3"]);
    instance.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____self____guest_sees_its_pid() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("")).unwrap();

    let reply = instance.call(text("self"), LONG).await.unwrap();

    let pid = instance.pid();
    assert_eq!(body(&reply), pid.text());
    instance.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____guest_traps____caller_gets_exit_reason_and_instance_fails() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("")).unwrap();

    let err = instance.call(text("trap"), LONG).await.unwrap_err();

    let CallError::Exited(ExitReason::Crashed(why)) = &err else {
        panic!("expected exited(crashed), got {err:?}");
    };
    assert!(why.contains("unreachable"), "{why}");
    assert_eq!(instance.wait().await, ExitReason::Crashed(why.clone()));
    assert_eq!(instance.state(), LifecycleState::Failed);
    assert!(rt.lookup(instance.pid()).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____guest_spins____killed_at_deadline_while_others_stay_responsive() {
    let (rt, app) = shared();
    let spinner = rt.spawn(app, spec("")).unwrap();
    let probe = rt.spawn(app, spec("")).unwrap();
    let started = Instant::now();

    let spin = tokio::spawn({
        let spinner = spinner.clone();
        async move { spinner.call(text("spin"), LONG).await }
    });
    let mut worst = Duration::ZERO;
    for _ in 0..10 {
        let t = Instant::now();
        probe.call(text("echo:x"), LONG).await.unwrap();
        worst = worst.max(t.elapsed());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let result = spin.await.unwrap();

    assert_eq!(result, Err(CallError::Exited(ExitReason::Timeout)));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert!(
        worst < Duration::from_millis(200),
        "probe latency {worst:?}"
    );
    probe.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____allocates_past_memory_max____crashes_with_memory_limit() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("")).unwrap();
    let neighbour = rt.spawn(app, spec("")).unwrap();

    let small = instance.call(text("alloc:4"), LONG).await;
    let big = instance.call(text("alloc:64"), LONG).await;

    assert_eq!(body(&small.unwrap()), "ok");
    assert_eq!(
        big,
        Err(CallError::Exited(ExitReason::Crashed(
            "memory_limit".into()
        )))
    );
    assert_eq!(
        body(&neighbour.call(text("echo:fine"), LONG).await.unwrap()),
        "fine"
    );
    neighbour.kill();
}

#[rstest]
#[case::returns_error("fail", ExitReason::Crashed("init failed: asked to fail".into()))]
#[case::spins("spin", ExitReason::Crashed("init_timeout".into()))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init____fails____exits_crashed(#[case] args: &str, #[case] expected: ExitReason) {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec(args)).unwrap();

    let reason = instance.wait().await;

    assert_eq!(reason, expected);
    assert_eq!(instance.state(), LifecycleState::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init____traps____exits_crashed_with_trap() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("trap")).unwrap();

    let reason = instance.wait().await;

    let ExitReason::Crashed(why) = reason else {
        panic!("expected crashed, got {reason:?}");
    };
    assert!(why.starts_with("trap:"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown____idle____runs_terminate_and_exits_with_reason() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("")).unwrap();
    instance.call(text("echo:up"), LONG).await.unwrap();

    instance.shutdown(ExitReason::Shutdown);
    let reason = instance.wait().await;

    assert_eq!(reason, ExitReason::Shutdown);
    assert_eq!(instance.state(), LifecycleState::Stopped);
    assert!(rt.lookup(instance.pid()).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown____terminate_overruns____killed_after_shutdown_timeout() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("terminate-spin")).unwrap();
    instance.call(text("echo:up"), LONG).await.unwrap();
    let started = Instant::now();

    instance.shutdown(ExitReason::Shutdown);
    let reason = instance.wait().await;

    assert_eq!(reason, ExitReason::Killed);
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(instance.state(), LifecycleState::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown____call_awaiting_deferred_reply____caller_gets_exited() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("")).unwrap();
    let pending = tokio::spawn({
        let instance = instance.clone();
        async move { instance.call(text("noreply"), LONG).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    instance.shutdown(ExitReason::Shutdown);
    let result = pending.await.unwrap();

    assert_eq!(result, Err(CallError::Exited(ExitReason::Shutdown)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill____during_callback____exits_killed_without_waiting_for_deadline() {
    let (rt, app) = shared();
    let mut long = spec("");
    long.callback_deadline = Duration::from_secs(30);
    let instance = rt.spawn(app, long).unwrap();
    let spin = tokio::spawn({
        let instance = instance.clone();
        async move { instance.call(text("spin"), LONG).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();

    instance.kill();
    let reason = instance.wait().await;

    assert_eq!(reason, ExitReason::Killed);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        spin.await.unwrap(),
        Err(CallError::Exited(ExitReason::Killed))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill____during_init____exits_killed() {
    let (rt, app) = shared();
    let mut long = spec("spin");
    long.init_timeout = Duration::from_secs(30);
    let instance = rt.spawn(app, long).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    instance.kill();
    let reason = instance.wait().await;

    assert_eq!(reason, ExitReason::Killed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mailbox____full____casts_and_calls_fail_fast_with_overloaded() {
    let (rt, app) = shared();
    let mut small = spec("");
    small.mailbox_max = 1;
    small.callback_deadline = Duration::from_secs(30);
    let instance = rt.spawn(app, small).unwrap();
    instance.call(text("echo:up"), LONG).await.unwrap();
    instance.cast(text("spin")).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    instance.cast(text("count")).unwrap();
    let started = Instant::now();

    let cast = instance.cast(text("count"));
    let call = instance.call(text("echo:x"), LONG).await;

    assert_eq!(cast, Err(CallError::Overloaded));
    assert_eq!(call, Err(CallError::Overloaded));
    assert!(started.elapsed() < Duration::from_millis(100));
    instance.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call____no_reply_in_time____timeout_and_instance_keeps_running() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("")).unwrap();

    let result = instance
        .call(text("noreply"), Duration::from_millis(100))
        .await;

    assert_eq!(result, Err(CallError::Timeout));
    assert_eq!(
        body(&instance.call(text("echo:still"), LONG).await.unwrap()),
        "still"
    );
    instance.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respawn____after_exit____same_id_next_incarnation_fresh_state() {
    let (rt, app) = shared();
    let first = rt.spawn(app, spec("")).unwrap();
    first.call(text("count"), LONG).await.unwrap();
    first.call(text("trap"), LONG).await.unwrap_err();
    first.wait().await;

    let second = rt.respawn(app, spec(""), first.pid()).unwrap();
    let count = second.call(text("count"), LONG).await.unwrap();

    assert_eq!(second.pid().id, first.pid().id);
    assert_eq!(second.pid().incarnation, first.pid().incarnation + 1);
    assert_eq!(body(&count), "1");
    assert!(rt.lookup(first.pid()).is_none());
    assert!(rt.lookup(second.pid()).is_some());
    second.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respawn____previous_still_running____refused() {
    let (rt, app) = shared();
    let first = rt.spawn(app, spec("")).unwrap();

    let result = rt.respawn(app, spec(""), first.pid());

    assert!(
        matches!(result, Err(paddock_host::HostError::StillRunning(pid)) if pid == first.pid())
    );
    first.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cast____after_exit____noproc() {
    let (rt, app) = shared();
    let instance = rt.spawn(app, spec("")).unwrap();
    instance.kill();
    instance.wait().await;

    let result = instance.cast(text("count"));

    assert_eq!(result, Err(CallError::Noproc));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_level_event____granted_logging____after_init_and_on_each_change() {
    let (rt, app) = shared();
    let mut granted = spec("");
    granted.app = "log-event".into();
    granted.granted.insert(Capability::Logging);
    granted.services.logging.level = LogLevel::Warn;
    let mut plain = spec("");
    plain.app = "log-event".into();
    let granted = rt.spawn(app, granted).unwrap();
    let plain = rt.spawn(app, plain).unwrap();

    let first = ask(&granted, "infos").await;
    rt.set_log_level("log-event", Some(LogLevel::Debug));
    let changed = ask(&granted, "infos").await;
    rt.set_log_level("log-event", None);
    let reset = ask(&granted, "infos").await;
    let ungranted = ask(&plain, "infos").await;

    assert_eq!(first, "event:log-level:warn");
    assert_eq!(changed, "event:log-level:warn;event:log-level:debug");
    assert_eq!(
        reset,
        "event:log-level:warn;event:log-level:debug;event:log-level:warn"
    );
    assert_eq!(ungranted, "");
    granted.kill();
    plain.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_level_event____after_snapshot_upgrade____sent_to_new_guest() {
    let (rt, app) = shared();
    let mut s = spec("");
    s.app = "log-event-upgrade".into();
    s.granted.insert(Capability::Logging);
    let h = rt.spawn(app, s.clone()).unwrap();
    ask(&h, "infos").await;

    s.version = "0.2.0".into();
    h.upgrade(app, s, "0.1.0").await.unwrap();
    let after = ask(&h, "infos").await;

    assert_eq!(after, "event:log-level:info");
    h.kill();
}
