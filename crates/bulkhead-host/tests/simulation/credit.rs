//! Credit for waited casts between nodes (ROADMAP I11 to I13): after one
//! waited cast from an instance is queued on the other node, the sender's
//! next ones go on credit without a round trip, in order; credit ends when
//! the target exits, the session ends or it lies unused, and a peer one
//! release older grants none. Each link here takes 10 ms each way, so a
//! round trip is about 20 ms.

use std::time::Duration;

use bulkhead_host::acl::Acl;
use bulkhead_host::transport::memory::Faults;
use bulkhead_host::transport::wire;
use bulkhead_host::{InstanceHandle, RuntimeConfig};

use crate::common::{ask, pid_text};
use crate::harness::{Sim, pass, simulate, simulate_with};

const ONE_WAY: Duration = Duration::from_millis(10);

fn slow_links(sim: &Sim) {
    let faults = Faults {
        delay: ONE_WAY,
        ..Faults::default()
    };
    sim.mesh.set_faults(1, 2, faults);
    sim.mesh.set_faults(2, 1, faults);
}

fn accepting(accept_from: &[&str]) -> Acl {
    Acl {
        accept_from: accept_from.iter().map(|a| (*a).to_string()).collect(),
        may_call: Vec::new(),
    }
}

/// A sender on node 1 and a target on node 2 that accepts it.
async fn pair(sim: &Sim, mailbox_max: Option<usize>) -> (InstanceHandle, InstanceHandle) {
    let sender = sim.node(1).spawn_as("sender", "", accepting(&[]));
    let target = sim.node(2).spawn_with(|s| {
        s.app = "target".into();
        s.acl = accepting(&["sender"]);
        if let Some(max) = mailbox_max {
            s.mailbox_max = max;
        }
    });
    pass(10).await;
    slow_links(sim);
    (sender, target)
}

/// `n` waited casts `seq:1` to `seq:<n>` from `sender` to `target`: how
/// many returned `Ok`, and the milliseconds they took.
async fn burst(sender: &InstanceHandle, target: &InstanceHandle, n: u32) -> (u32, u64) {
    let answer = ask(
        sender,
        &format!("cast-waits:{}:2000:{n}", pid_text(target.pid())),
    )
    .await;
    let (ok, ms) = answer
        .strip_prefix("ok:")
        .and_then(|rest| rest.split_once(" ms:"))
        .unwrap_or_else(|| panic!("{answer}"));
    (ok.parse().unwrap(), ms.parse().unwrap())
}

#[test]
fn credit____burst_from_an_instance____one_round_trip_then_in_order() {
    let ((ok, ms), seq) = simulate(91, 2, |sim| async move {
        let (sender, target) = pair(&sim, Some(1_000)).await;

        let sent = burst(&sender, &target, 50).await;
        pass(100).await;
        (sent, ask(&target, "seq").await)
    });

    assert_eq!(ok, 50);
    assert!(ms < 60, "50 waited casts took {ms} ms");
    assert_eq!(seq, "last:50 errors:0");
}

#[test]
fn credit____none_configured____a_round_trip_each() {
    let off = |_: u32, c: &mut RuntimeConfig| c.cast_credit = 0;
    let ((ok, ms), seq) = simulate_with(92, 2, off, |sim| async move {
        let (sender, target) = pair(&sim, None).await;

        let sent = burst(&sender, &target, 10).await;
        pass(100).await;
        (sent, ask(&target, "seq").await)
    });

    assert_eq!(ok, 10);
    assert!(ms >= 200, "10 waited casts took {ms} ms");
    assert_eq!(seq, "last:10 errors:0");
}

#[test]
fn credit____peer_one_release_older____none_granted_and_every_cast_arrives() {
    let ((ok, ms), seq) = simulate(93, 2, |sim| async move {
        sim.mesh.set_version(2, wire::VERSION - 1);
        let (sender, target) = pair(&sim, None).await;

        let sent = burst(&sender, &target, 10).await;
        pass(100).await;
        (sent, ask(&target, "seq").await)
    });

    assert_eq!(ok, 10);
    assert!(ms >= 200, "10 waited casts took {ms} ms");
    assert_eq!(seq, "last:10 errors:0");
}

/// With a mailbox of 4 and the target busy, the credit holds at most half
/// of it; the rest of the casts wait for room on the other node, and all of
/// them arrive in order.
#[test]
fn credit____busy_target_with_a_small_mailbox____the_rest_wait_and_all_arrive_in_order() {
    let ((ok, ms), seq) = simulate(94, 2, |sim| async move {
        let (sender, target) = pair(&sim, Some(4)).await;
        let stuck = sim.node(2).spawn("");
        pass(10).await;
        target
            .cast(crate::common::text(&format!(
                "call:{}:300:noreply",
                pid_text(stuck.pid())
            )))
            .unwrap();
        pass(5).await;

        let sent = burst(&sender, &target, 12).await;
        pass(100).await;
        (sent, ask(&target, "seq").await)
    });

    assert_eq!(ok, 12);
    assert!(ms >= 250, "the casts waited for the busy target: {ms} ms");
    assert_eq!(seq, "last:12 errors:0");
}

#[test]
fn credit____target_exits____voided_and_the_next_cast_noproc() {
    let (first, after) = simulate(95, 2, |sim| async move {
        let (sender, target) = pair(&sim, None).await;
        let first = burst(&sender, &target, 3).await;
        target.kill();
        pass(100).await;

        let wait = format!("cast-wait:{}:1000:seq:4", pid_text(target.pid()));
        (first, ask(&sender, &wait).await)
    });

    assert_eq!(first.0, 3);
    assert_eq!(after, "err:noproc");
}

#[test]
fn credit____session_ends____dropped_and_the_next_cast_a_round_trip() {
    let ((ok, ms), seq) = simulate(96, 2, |sim| async move {
        let (sender, target) = pair(&sim, None).await;
        burst(&sender, &target, 3).await;
        pass(100).await;
        sim.mesh.partition(1, 2);
        pass(6_000).await;
        sim.mesh.heal(1, 2);
        slow_links(&sim);
        pass(3_000).await;
        let reset = ask(&target, "seq").await;
        assert_eq!(reset, "last:3 errors:0");

        let sent = burst(&sender, &target, 1).await;
        pass(100).await;
        (sent, ask(&target, "seq").await)
    });

    assert_eq!(ok, 1);
    assert!(ms >= 20, "a new session starts without credit: {ms} ms");
    // The burst restarts at 1: one cast out of order.
    assert_eq!(seq, "last:1 errors:1");
}

#[test]
fn credit____unused_for_a_second____given_back_and_the_next_cast_a_round_trip() {
    let (warm, cold) = simulate(97, 2, |sim| async move {
        let (sender, target) = pair(&sim, None).await;
        burst(&sender, &target, 1).await;

        let warm = burst(&sender, &target, 1).await;
        pass(2_000).await;
        let cold = burst(&sender, &target, 1).await;
        (warm, cold)
    });

    assert_eq!((warm.0, cold.0), (1, 1));
    assert!(warm.1 < 5, "credit held: {} ms", warm.1);
    assert!(cold.1 >= 20, "credit given back: {} ms", cold.1);
}
