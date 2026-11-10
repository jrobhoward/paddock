//! A cast that waits on another node for room in a full mailbox
//! (`Frame::CastWait`, ROADMAP I3): queued once room comes; a timeout the
//! receiving node answers, with nothing queued; `overloaded` past the
//! link's budget; `noconnection` when the session ends meanwhile or the
//! peer's version cannot read the frame; and from an instance, checked by
//! the target's messaging ACL on its own node (ROADMAP I8).

use std::time::Duration;

use bulkhead_host::acl::Acl;
use bulkhead_host::transport::wire;
use bulkhead_host::{CallError, InstanceHandle};
use tokio::time::Instant;

use crate::common::{ask, pid_text, text};
use crate::harness::{Sim, pass, simulate};

/// On node 2: an instance with a one-message mailbox, held for `busy_ms`
/// in a call nobody answers, its mailbox then filled.
async fn full_target(sim: &Sim, busy_ms: u64) -> InstanceHandle {
    let stuck = sim.node(2).spawn("");
    let b = sim.node(2).spawn_with(|s| s.mailbox_max = 1);
    pass(10).await;
    let hold = format!("call:{}:{busy_ms}:noreply", pid_text(stuck.pid()));
    b.cast(text(&hold)).unwrap();
    pass(5).await;
    b.cast(text("echo:filler")).unwrap();
    b
}

#[test]
fn cast_wait____room_comes_on_the_other_node____queued() {
    let (sent, waited, count) = simulate(81, 2, |sim| async move {
        let b = full_target(&sim, 300).await;

        let started = Instant::now();
        let sent = sim
            .node(1)
            .rt
            .cast_timeout(b.pid(), text("count"), Duration::from_secs(2))
            .await;
        let waited = started.elapsed();
        pass(50).await;
        (sent, waited, ask(&b, "count").await)
    });

    assert_eq!(sent, Ok(()));
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert_eq!(count, "2", "the waited cast was handled before the ask");
}

#[test]
fn cast_wait____no_room_in_time____timeout_answered_and_nothing_queued() {
    let (sent, waited, count) = simulate(82, 2, |sim| async move {
        let b = full_target(&sim, 2000).await;

        let started = Instant::now();
        let sent = sim
            .node(1)
            .rt
            .cast_timeout(b.pid(), text("count"), Duration::from_millis(200))
            .await;
        let waited = started.elapsed();
        pass(2500).await;
        (sent, waited, ask(&b, "count").await)
    });

    assert_eq!(sent, Err(CallError::Timeout));
    // The receiving node's answer, before the sender's own grace ran out.
    assert!(waited < Duration::from_millis(400), "{waited:?}");
    assert_eq!(count, "1", "the timed-out cast was never queued");
}

#[test]
fn cast_wait____link_past_its_budget____overloaded_at_once() {
    let (sent, waited) = simulate(83, 2, |sim| async move {
        let b = sim.node(2).spawn("");
        pass(10).await;
        sim.mesh.set_send_budget(1_000);
        sim.mesh.stall_one_way(1, 2);
        let padding = "x".repeat(200);
        for _ in 0..10 {
            let _ = sim
                .node(1)
                .rt
                .cast(b.pid(), text(&format!("echo:{padding}")));
        }

        let started = Instant::now();
        let sent = sim
            .node(1)
            .rt
            .cast_timeout(b.pid(), text("count"), Duration::from_secs(2))
            .await;
        let waited = started.elapsed();
        sim.mesh.heal(1, 2);
        (sent, waited)
    });

    assert_eq!(sent, Err(CallError::Overloaded));
    assert_eq!(waited, Duration::ZERO);
}

#[test]
fn cast_wait____session_ends_while_waiting____noconnection() {
    let sent = simulate(84, 2, |sim| async move {
        let b = full_target(&sim, 5000).await;
        let rt = &sim.node(1).rt;

        let (sent, ()) = tokio::join!(
            rt.cast_timeout(b.pid(), text("count"), Duration::from_secs(3)),
            async {
                pass(50).await;
                sim.mesh.partition(1, 2);
            }
        );
        sent
    });

    assert_eq!(sent, Err(CallError::Noconnection));
}

#[test]
fn cast_wait____peer_one_release_older____noconnection_and_plain_casts_still_go() {
    let (waited, plain) = simulate(85, 2, |sim| async move {
        sim.mesh.set_version(2, wire::VERSION - 1);
        let b = sim.node(2).spawn("");
        pass(10).await;

        let waited = sim
            .node(1)
            .rt
            .cast_timeout(b.pid(), text("count"), Duration::from_secs(1))
            .await;
        let plain = sim.node(1).rt.cast(b.pid(), text("count"));
        (waited, plain)
    });

    assert_eq!(waited, Err(CallError::Noconnection));
    assert_eq!(plain, Ok(()));
}

/// Messaging routes that accept casts from `accept_from` only.
fn accepting(accept_from: &[&str]) -> Acl {
    Acl {
        accept_from: accept_from.iter().map(|a| (*a).to_string()).collect(),
        may_call: Vec::new(),
    }
}

/// From an instance on node 1 to one on node 2 that accepts its app: the
/// waited cast is queued and handled.
#[test]
fn cast_wait____from_an_instance_the_target_accepts____queued_on_the_other_node() {
    let (sent, count) = simulate(86, 2, |sim| async move {
        let sender = sim.node(1).spawn_as("sender", "", accepting(&[]));
        let target = sim.node(2).spawn_as("target", "", accepting(&["sender"]));
        pass(10).await;

        let wait = format!("cast-wait:{}:1000:count", pid_text(target.pid()));
        let sent = ask(&sender, &wait).await;
        pass(50).await;
        (sent, ask(&target, "count").await)
    });

    assert_eq!(sent, "ok");
    assert_eq!(count, "2", "the waited cast was handled before the ask");
}

/// The target's node checks the sender's app against its `accept_from`, as
/// for a call: refused, the waited cast answers `access-denied` and nothing
/// is queued.
#[test]
fn cast_wait____from_an_app_the_target_does_not_accept____access_denied_and_nothing_queued() {
    let (sent, count) = simulate(87, 2, |sim| async move {
        let sender = sim.node(1).spawn_as("sender", "", accepting(&[]));
        let target = sim
            .node(2)
            .spawn_as("guarded", "", accepting(&["someone-else"]));
        pass(10).await;

        let wait = format!("cast-wait:{}:1000:count", pid_text(target.pid()));
        let sent = ask(&sender, &wait).await;
        pass(50).await;
        (sent, ask(&target, "count").await)
    });

    assert_eq!(sent, "err:access-denied");
    assert_eq!(count, "1", "nothing was queued");
}
