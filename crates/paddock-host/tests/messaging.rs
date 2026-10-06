//! Messaging between instances: guest-side `cast`, `call` and `reply`,
//! timers, the registry and message limits, against the chaos fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{LONG, body, pid_text, shared, spec, text};
use paddock_host::{CallError, Envelope, ExitReason, InstanceSpec, Pid};

async fn ask(instance: &paddock_host::InstanceHandle, command: &str) -> String {
    body(&instance.call(text(command), LONG).await.unwrap())
}

fn named(names: &[&str]) -> InstanceSpec {
    let mut s = spec("");
    s.registered_names = names.iter().map(ToString::to_string).collect();
    s
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_call____to_another_instance____returns_its_reply() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();

    let reply = ask(&a, &format!("call:{}:1000:echo:hi", pid_text(b.pid()))).await;

    assert_eq!(reply, "ok:hi");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_call____unknown_or_stale_pid____noproc() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    b.kill();
    b.wait().await;
    let stale = b.pid();
    let unknown = Pid {
        node: 0,
        id: u64::MAX,
        incarnation: 1,
    };

    let to_stale = ask(&a, &format!("call:{}:1000:echo:x", pid_text(stale))).await;
    let to_unknown = ask(&a, &format!("call:{}:1000:echo:x", pid_text(unknown))).await;

    assert_eq!(to_stale, "err:noproc");
    assert_eq!(to_unknown, "err:noproc");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_call____target_dies_mid_call____caller_gets_reason_and_keeps_running() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let mut slow = spec("");
    slow.callback_deadline = Duration::from_millis(800);
    let b = rt.spawn(app, slow).unwrap();
    let started = Instant::now();

    let reply = ask(&a, &format!("call:{}:5000:spin", pid_text(b.pid()))).await;

    // A's own deadline is 300 ms; waiting 800 ms on B is host time, not
    // runaway code, so A survives.
    assert_eq!(reply, "err:exited:timeout");
    assert!(started.elapsed() >= Duration::from_millis(800));
    assert_eq!(ask(&a, "count").await, "1");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_call____no_reply_in_time____timeout_and_late_reply_dropped() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();

    let reply = ask(&a, &format!("call:{}:100:defer", pid_text(b.pid()))).await;
    let late = ask(&b, "answer:late").await;

    assert_eq!(reply, "err:timeout");
    assert_eq!(late, "answered");
    assert_eq!(ask(&a, "echo:fine").await, "fine");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply____after_noreply____answers_the_deferred_caller() {
    let (rt, app) = shared();
    let b = rt.spawn(app, spec("")).unwrap();
    let deferred = tokio::spawn({
        let b = b.clone();
        async move { b.call(text("defer"), LONG).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let answered = ask(&b, "answer:later").await;
    let reply = deferred.await.unwrap().unwrap();

    assert_eq!(answered, "answered");
    assert_eq!(body(&reply), "later");
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply____during_the_callback____answers_the_caller() {
    let (rt, app) = shared();
    let b = rt.spawn(app, spec("")).unwrap();

    let reply = ask(&b, "answer-now:early").await;

    assert_eq!(reply, "early");
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_call____to_itself____caller_crashes() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();

    let result = a.call(text("call-self"), LONG).await;

    let Err(CallError::Exited(ExitReason::Crashed(why))) = &result else {
        panic!("expected exited(crashed), got {result:?}");
    };
    assert!(why.contains("call to self"), "{why}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_cast____to_another_instance____delivered_before_later_messages() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();

    let sent = ask(&a, &format!("cast:{}:count", pid_text(b.pid()))).await;
    let count = ask(&b, "count").await;

    assert_eq!(sent, "sent");
    assert_eq!(count, "2");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_cast____to_dead_pid____dropped_silently() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();

    let sent = ask(&a, "cast:0.18446744073709551615.1:count").await;

    assert_eq!(sent, "sent");
    assert_eq!(ask(&a, "echo:alive").await, "alive");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_max_bytes____body_too_large____refused_with_too_large() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let mut tight = spec("");
    tight.message_max_bytes = 16;
    let b = rt.spawn(app, tight).unwrap();
    let big = "echo:".to_string() + &"x".repeat(32);

    let cast = b.cast(text(&big));
    let call = b.call(text(&big), LONG).await;
    let from_guest = ask(&a, &format!("call:{}:1000:{big}", pid_text(b.pid()))).await;
    let small = ask(&b, "echo:ok").await;

    assert_eq!(cast, Err(CallError::TooLarge));
    assert_eq!(call, Err(CallError::TooLarge));
    assert_eq!(from_guest, "err:too-large");
    assert_eq!(small, "ok");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mailbox____full____dropped_casts_are_counted() {
    let (rt, app) = shared();
    let mut small = spec("");
    small.mailbox_max = 1;
    small.callback_deadline = Duration::from_secs(30);
    let b = rt.spawn(app, small).unwrap();
    b.call(text("echo:up"), LONG).await.unwrap();
    b.cast(text("spin")).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    b.cast(text("count")).unwrap();

    let first = b.cast(text("count"));
    let second = b.cast(text("count"));

    assert_eq!(first, Err(CallError::Overloaded));
    assert_eq!(second, Err(CallError::Overloaded));
    assert_eq!(b.dropped_messages(), 2);
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_after____timers_fire____delivered_to_handle_info_in_order() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();

    ask(&a, "timer:100:second").await;
    ask(&a, "timer:20:first").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let fired = ask(&a, "timers").await;

    assert_eq!(fired, "first,second");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_timer____before_it_fires____never_delivered() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();

    let reference = ask(&a, "timer:100:never").await;
    ask(&a, &format!("cancel:{reference}")).await;
    ask(&a, "timer:20:kept").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let fired = ask(&a, "timers").await;

    assert_eq!(fired, "kept");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register____listed_name____whereis_finds_it_from_host_and_guest() {
    let (rt, app) = shared();
    let a = rt.spawn(app, named(&["reg-found"])).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();

    let registered = ask(&a, "register:reg-found").await;
    let from_guest = ask(&b, "whereis:reg-found").await;

    assert_eq!(registered, "ok");
    assert_eq!(from_guest, pid_text(a.pid()));
    assert_eq!(rt.whereis("reg-found").unwrap().pid(), a.pid());
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register____name_not_in_manifest____access_denied() {
    let (rt, app) = shared();
    let a = rt.spawn(app, named(&["reg-allowed"])).unwrap();

    let result = ask(&a, "register:reg-other").await;

    assert_eq!(result, "err:access-denied");
    assert!(rt.whereis("reg-other").is_none());
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register____name_held_by_live_instance____taken_with_holder() {
    let (rt, app) = shared();
    let a = rt.spawn(app, named(&["reg-taken"])).unwrap();
    let b = rt.spawn(app, named(&["reg-taken"])).unwrap();
    ask(&a, "register:reg-taken").await;

    let result = ask(&b, "register:reg-taken").await;

    assert_eq!(result, format!("err:taken:{}", pid_text(a.pid())));
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register____holder_exits____name_released_for_next_incarnation() {
    let (rt, app) = shared();
    let mut s = named(&["reg-restart"]);
    s.init_args = b"register:reg-restart".to_vec();
    let first = rt.spawn(app, s.clone()).unwrap();
    ask(&first, "echo:up").await;
    first.call(text("trap"), LONG).await.unwrap_err();
    first.wait().await;

    let released = rt.whereis("reg-restart");
    let second = rt.respawn(app, s, first.pid()).unwrap();
    ask(&second, "echo:up").await;

    assert!(released.is_none());
    assert_eq!(rt.whereis("reg-restart").unwrap().pid(), second.pid());
    second.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn envelope____content_type_and_schema____reach_the_guest_untouched() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let sent = Envelope {
        content_type: "application/x-paddock-test".into(),
        schema: Some("orders.place.v1".into()),
        body: b"mirror".to_vec(),
    };

    let reply = a.call(sent.clone(), LONG).await.unwrap();

    assert_eq!(reply, sent);
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_call____stale_pid____noproc() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    a.kill();
    a.wait().await;

    let call = rt.call(a.pid(), text("echo:x"), LONG).await;
    let cast = rt.cast(a.pid(), text("count"));

    assert_eq!(call, Err(CallError::Noproc));
    assert_eq!(cast, Err(CallError::Noproc));
}
