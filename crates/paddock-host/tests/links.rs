//! Links, monitors, trap-exits and exit signals, against the chaos fixture.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{LONG, ask, ask_until, pid_text, shared, spec, text};
use paddock_host::{ExitReason, InstanceHandle};

fn crash_reason(reason: &ExitReason) -> String {
    match reason {
        ExitReason::Crashed(why) => format!("crashed({why})"),
        other => other.to_string(),
    }
}

async fn crash(instance: &InstanceHandle) -> ExitReason {
    instance.call(text("trap"), LONG).await.unwrap_err();
    instance.wait().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____partner_crashes____non_trapping_instance_exits_with_same_reason() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    ask(&a, &format!("link:{}", pid_text(b.pid()))).await;

    let reason = crash(&b).await;

    assert_eq!(a.wait().await, reason);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____either_side_crashes____propagates_both_ways() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    ask(&a, &format!("link:{}", pid_text(b.pid()))).await;

    let reason = crash(&a).await;

    assert_eq!(b.wait().await, reason);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____partner_crashes____trapping_instance_gets_info_exit_and_survives() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("trap-exits")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    ask(&a, &format!("link:{}", pid_text(b.pid()))).await;

    let reason = crash(&b).await;
    let infos = ask_until(&a, "infos", |s| !s.is_empty()).await;

    assert_eq!(
        infos,
        format!("exit:{}:{}", pid_text(b.pid()), crash_reason(&reason))
    );
    assert!(a.exit_reason().is_none());
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____partner_exits_normal____non_trapping_instance_keeps_running() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    ask(&a, &format!("link:{}", pid_text(b.pid()))).await;

    ask(&b, &format!("exit:{}:normal", pid_text(b.pid()))).await;
    let reason = b.wait().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(reason, ExitReason::Normal);
    assert_eq!(ask(&a, "echo:alive").await, "alive");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____partner_shut_down____non_trapping_instance_exits_with_shutdown() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    ask(&a, &format!("link:{}", pid_text(b.pid()))).await;

    b.shutdown(ExitReason::Shutdown);

    assert_eq!(b.wait().await, ExitReason::Shutdown);
    assert_eq!(a.wait().await, ExitReason::Shutdown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____to_dead_pid____non_trapping_caller_exits_noproc() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    b.kill();
    b.wait().await;

    let _ = a
        .call(text(&format!("link:{}", pid_text(b.pid()))), LONG)
        .await;

    assert_eq!(a.wait().await, ExitReason::Noproc);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____to_dead_pid____trapping_caller_gets_info_exit_noproc() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("trap-exits")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    b.kill();
    b.wait().await;

    ask(&a, &format!("link:{}", pid_text(b.pid()))).await;
    let infos = ask_until(&a, "infos", |s| !s.is_empty()).await;

    assert_eq!(infos, format!("exit:{}:noproc", pid_text(b.pid())));
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink____then_partner_crashes____no_propagation() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    ask(&a, &format!("link:{}", pid_text(b.pid()))).await;
    ask(&b, &format!("unlink:{}", pid_text(a.pid()))).await;

    crash(&b).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(ask(&a, "echo:alive").await, "alive");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor____target_crashes____watcher_gets_down_with_reason() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    let reference = ask(&a, &format!("monitor:{}", pid_text(b.pid()))).await;

    let reason = crash(&b).await;
    let infos = ask_until(&a, "infos", |s| !s.is_empty()).await;

    assert_eq!(
        infos,
        format!(
            "down:{reference}:{}:{}",
            pid_text(b.pid()),
            crash_reason(&reason)
        )
    );
    assert!(a.exit_reason().is_none());
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor____dead_pid____down_noproc_at_once() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    b.kill();
    b.wait().await;

    let reference = ask(&a, &format!("monitor:{}", pid_text(b.pid()))).await;
    let infos = ask_until(&a, "infos", |s| !s.is_empty()).await;

    assert_eq!(
        infos,
        format!("down:{reference}:{}:noproc", pid_text(b.pid()))
    );
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demonitor____then_target_crashes____no_down() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    let reference = ask(&a, &format!("monitor:{}", pid_text(b.pid()))).await;
    ask(&a, &format!("demonitor:{reference}")).await;

    crash(&b).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(ask(&a, "infos").await, "");
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_signal____killed_to_trapping_instance____untrappable() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("trap-exits")).unwrap();
    ask(&b, "echo:up").await;

    ask(&a, &format!("exit:{}:killed", pid_text(b.pid()))).await;

    assert_eq!(b.wait().await, ExitReason::Killed);
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_signal____abnormal_to_non_trapping____target_exits_with_reason() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();

    ask(&a, &format!("exit:{}:crashed-told", pid_text(b.pid()))).await;

    assert_eq!(b.wait().await, ExitReason::Crashed("told".into()));
    a.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_signal____to_trapping____arrives_as_info_exit() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("trap-exits")).unwrap();
    // Trapping starts in `init`; a signal that arrives earlier stops it.
    ask(&b, "echo:up").await;

    ask(&a, &format!("exit:{}:shutdown", pid_text(b.pid()))).await;
    let infos = ask_until(&b, "infos", |s| !s.is_empty()).await;

    assert_eq!(infos, format!("exit:{}:shutdown", pid_text(a.pid())));
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_signal____normal_to_non_trapping_other____ignored() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();

    ask(&a, &format!("exit:{}:normal", pid_text(b.pid()))).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(ask(&b, "echo:alive").await, "alive");
    a.kill();
    b.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link____restarted_instance____does_not_inherit_links() {
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("")).unwrap();
    let b = rt.spawn(app, spec("")).unwrap();
    ask(&a, &format!("link:{}", pid_text(b.pid()))).await;
    a.kill();
    a.wait().await;
    b.wait().await;
    let a2 = rt.respawn(app, spec(""), a.pid()).unwrap();
    let c = rt.spawn(app, spec("")).unwrap();
    ask(&c, &format!("link:{}", pid_text(a2.pid()))).await;

    crash(&c).await;

    assert!(a2.wait().await != ExitReason::Normal);
    assert_eq!(b.exit_reason(), Some(ExitReason::Killed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_pid____no_transport____noconnection_everywhere() {
    // A pid on another node: unreachable until a transport exists.
    let (rt, app) = shared();
    let a = rt.spawn(app, spec("trap-exits")).unwrap();
    let remote = "7.1.1";

    let call = ask(&a, &format!("call:{remote}:500:echo:x")).await;
    let cast = ask(&a, &format!("cast:{remote}:count")).await;
    let link = ask(&a, &format!("link:{remote}")).await;
    let reference = ask(&a, &format!("monitor:{remote}")).await;
    let infos = ask_until(&a, "infos", |s| s.contains("down:")).await;
    let from_host = rt
        .call(
            paddock_host::Pid {
                node: 7,
                id: 1,
                incarnation: 1,
            },
            text("echo:x"),
            LONG,
        )
        .await;

    assert_eq!(call, "err:noconnection");
    assert_eq!(cast, "sent");
    assert_eq!(link, "ok");
    assert_eq!(
        infos,
        format!("exit:{remote}:noconnection;down:{reference}:{remote}:noconnection")
    );
    assert_eq!(from_host, Err(paddock_host::CallError::Noconnection));
    a.kill();
}
