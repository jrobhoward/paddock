//! Host-object quotas (ROADMAP H1): timers, monitors, links and pending
//! calls per instance, against the chaos fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use common::{LONG, ask, ask_until, pid_text, shared, spec, text};
use paddock_host::{CallError, ExitReason, HostError, HostObjectRequest, InstanceSpec};

fn limited(request: HostObjectRequest) -> InstanceSpec {
    InstanceSpec {
        host_objects: request,
        ..spec("")
    }
}

fn crashed(why: &str) -> ExitReason {
    ExitReason::Crashed(why.to_string())
}

async fn refused(instance: &paddock_host::InstanceHandle, command: &str) -> CallError {
    instance.call(text(command), LONG).await.unwrap_err()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_after____over_timers_max____crashes_with_timer_limit() {
    let (rt, app) = shared();
    let a = rt
        .spawn(
            app,
            limited(HostObjectRequest {
                timers: Some(2),
                ..HostObjectRequest::default()
            }),
        )
        .unwrap();
    ask(&a, "timer:60000:x").await;
    ask(&a, "timer:60000:y").await;

    let err = refused(&a, "timer:60000:z").await;

    assert_eq!(err, CallError::Exited(crashed("timer_limit")));
    assert_eq!(a.wait().await, crashed("timer_limit"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_after____cancelled_or_fired_timer____frees_its_slot() {
    let (rt, app) = shared();
    let a = rt
        .spawn(
            app,
            limited(HostObjectRequest {
                timers: Some(1),
                ..HostObjectRequest::default()
            }),
        )
        .unwrap();
    let reference = ask(&a, "timer:60000:x").await;
    ask(&a, &format!("cancel:{reference}")).await;
    ask(&a, "timer:1:fired").await;
    ask_until(&a, "timers", |r| r == "fired").await;

    let again = ask(&a, "timer:60000:y").await;

    assert!(again.parse::<u64>().is_ok(), "{again}");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor____over_monitors_max____crashes_with_monitor_limit() {
    let (rt, app) = shared();
    let a = rt
        .spawn(
            app,
            limited(HostObjectRequest {
                monitors: Some(1),
                ..HostObjectRequest::default()
            }),
        )
        .unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    let c = rt.spawn(app, spec("")).unwrap();
    let first = ask(&a, &format!("monitor:{}", pid_text(b.pid()))).await;
    ask(&a, &format!("demonitor:{first}")).await;
    ask(&a, &format!("monitor:{}", pid_text(b.pid()))).await;

    let err = refused(&a, &format!("monitor:{}", pid_text(c.pid()))).await;

    assert_eq!(err, CallError::Exited(crashed("monitor_limit")));
    b.kill();
    c.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____over_links_max____crashes_with_link_limit() {
    let (rt, app) = shared();
    let a = rt
        .spawn(
            app,
            limited(HostObjectRequest {
                links: Some(1),
                ..HostObjectRequest::default()
            }),
        )
        .unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    let c = rt.spawn(app, spec("")).unwrap();
    ask(&b, "trap-exits:on").await;
    ask(&a, &format!("link:{}", pid_text(b.pid()))).await;
    // Linking again with the same instance holds no new link.
    ask(&a, &format!("link:{}", pid_text(b.pid()))).await;

    let err = refused(&a, &format!("link:{}", pid_text(c.pid()))).await;

    assert_eq!(err, CallError::Exited(crashed("link_limit")));
    b.kill();
    c.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____made_by_others____never_refused() {
    let (rt, app) = shared();
    let a = rt
        .spawn(
            app,
            limited(HostObjectRequest {
                links: Some(1),
                ..HostObjectRequest::default()
            }),
        )
        .unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    let c = rt.spawn(app, spec("")).unwrap();
    ask(&a, "trap-exits:on").await;
    ask(&b, "trap-exits:on").await;
    ask(&c, "trap-exits:on").await;

    ask(&b, &format!("link:{}", pid_text(a.pid()))).await;
    ask(&c, &format!("link:{}", pid_text(a.pid()))).await;

    assert_eq!(ask(&a, "count").await, "1");
    a.kill();
    b.kill();
    c.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noreply____over_pending_calls_max____crashes_with_pending_call_limit() {
    let (rt, app) = shared();
    let a = rt
        .spawn(
            app,
            limited(HostObjectRequest {
                pending_calls: Some(1),
                ..HostObjectRequest::default()
            }),
        )
        .unwrap();
    // Whichever deferred call is handled second crashes the instance, and
    // both callers learn why.
    let first = {
        let a = a.clone();
        tokio::spawn(async move { a.call(text("defer"), LONG).await })
    };

    let second = a.call(text("defer"), LONG).await;

    let pending_crash = CallError::Exited(crashed("pending_call_limit"));
    assert_eq!(second.unwrap_err(), pending_crash);
    assert_eq!(first.await.unwrap().unwrap_err(), pending_crash);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn____above_node_limit____refused() {
    let (rt, app) = shared();

    let result = rt.spawn(
        app,
        limited(HostObjectRequest {
            monitors: Some(1_000_000),
            ..HostObjectRequest::default()
        }),
    );

    assert!(
        matches!(&result, Err(HostError::HostObjectsAboveNode(m)) if m.contains("monitors_max")),
        "{result:?}"
    );
}
