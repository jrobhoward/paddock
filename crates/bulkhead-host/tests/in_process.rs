//! The in-process test kind (ROADMAP Z9c): the chaos app run in its
//! instances' tasks, over the native protocol's frames, under the native
//! kind's lifecycle; and under tokio's paused clock, which a process
//! cannot follow.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use bulkhead_host::in_process::chaos::Chaos;
use bulkhead_host::{ExitReason, InstanceHandle, LoadedApp, Runtime, RuntimeConfig};
use common::{ask, ask_until, pid_text, spec, text};

fn runtime() -> (Runtime, LoadedApp) {
    let rt = Runtime::new(RuntimeConfig::default()).unwrap();
    let app = rt.load_in_process::<Chaos>();
    (rt, app)
}

fn start(rt: &Runtime, app: &LoadedApp, args: &str) -> InstanceHandle {
    rt.spawn(app, spec(args)).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process____echo_and_count____answered_in_the_instance_task() {
    let (rt, app) = runtime();
    let instance = start(&rt, &app, "");

    let echo = ask(&instance, "echo:hi").await;
    let first = ask(&instance, "count").await;
    let second = ask(&instance, "count").await;
    let me = ask(&instance, "self").await;

    assert_eq!(echo, "hi");
    assert_eq!((first.as_str(), second.as_str()), ("1", "2"));
    assert_eq!(me, pid_text(instance.pid()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process____calls____to_another_answered_to_itself_crash() {
    let (rt, app) = runtime();
    let a = start(&rt, &app, "");
    let b = start(&rt, &app, "");

    let reply = ask(&a, &format!("call:{}:1000:echo:there", pid_text(b.pid()))).await;
    let to_self = a.call(text("call-self"), common::LONG).await;

    assert_eq!(reply, "ok:there");
    // A call to itself would wait on its own mailbox: the caller crashes.
    let Err(bulkhead_host::CallError::Exited(ExitReason::Crashed(why))) = &to_self else {
        panic!("expected exited(crashed), got {to_self:?}");
    };
    assert!(why.contains("call to self"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process____defer_then_answer____the_caller_gets_the_later_reply() {
    let (rt, app) = runtime();
    let instance = start(&rt, &app, "");
    let caller = instance.clone();

    let waiting = tokio::spawn(async move { caller.call(text("defer"), common::LONG).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let answered = ask(&instance, "answer:later").await;
    let late = waiting.await.unwrap().unwrap();

    assert_eq!(answered, "answered");
    assert_eq!(common::body(&late), "later");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process____trap____crashed_as_a_native_panic() {
    let (rt, app) = runtime();
    let instance = start(&rt, &app, "");

    let _ = instance.call(text("trap"), common::LONG).await;
    let reason = instance.wait().await;

    assert_eq!(reason, ExitReason::Crashed("panic: asked to trap".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process____linked_instance_crashes____trapping_one_gets_the_exit() {
    let (rt, app) = runtime();
    let watcher = start(&rt, &app, "trap-exits");
    let doomed = start(&rt, &app, "");

    ask(&watcher, &format!("link:{}", pid_text(doomed.pid()))).await;
    let reference = ask(&watcher, &format!("monitor:{}", pid_text(doomed.pid()))).await;
    let _ = doomed.call(text("trap"), common::LONG).await;
    let infos = ask_until(&watcher, "infos", |i| i.contains("down")).await;

    let doomed_pid = pid_text(doomed.pid());
    assert!(
        infos.contains(&format!("exit:{doomed_pid}:crashed(panic: asked to trap)")),
        "{infos}"
    );
    assert!(
        infos.contains(&format!(
            "down:{reference}:{doomed_pid}:crashed(panic: asked to trap)"
        )),
        "{infos}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process____timer____fires_as_an_info() {
    let (rt, app) = runtime();
    let instance = start(&rt, &app, "");

    ask(&instance, "timer:20:ding").await;
    let fired = ask_until(&instance, "timers", |t| !t.is_empty()).await;

    assert_eq!(fired, "ding");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process____init_fails____start_refused_with_the_reason() {
    let (rt, app) = runtime();

    let instance = start(&rt, &app, "fail");
    let reason = instance.wait().await;

    assert_eq!(
        reason,
        ExitReason::Crashed("init failed: asked to fail".into())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process____killed____exits_killed() {
    let (rt, app) = runtime();
    let instance = start(&rt, &app, "");
    ask(&instance, "echo:up").await;

    instance.kill();
    let reason = instance.wait().await;

    assert_eq!(reason, ExitReason::Killed);
}

/// The simulation's clock: paused, moving only when every task is idle.
/// An in-process app adds no thread, so an hour-long timer fires when the
/// clock is advanced, not before, and the instance's own deadlines do not
/// run out while it works.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn in_process____paused_clock____timers_follow_it() {
    let (rt, app) = runtime();
    let instance = start(&rt, &app, "");

    ask(&instance, "timer:3600000:an-hour").await;
    let before = ask(&instance, "timers").await;
    tokio::time::advance(Duration::from_secs(3600)).await;
    let after = ask_until(&instance, "timers", |t| !t.is_empty()).await;
    let echo = ask(&instance, "echo:still-here").await;

    assert_eq!(before, "");
    assert_eq!(after, "an-hour");
    assert_eq!(echo, "still-here");
}
