//! Races between links, monitors, exits, partitions and heals across nodes,
//! on a paused clock, so each test sees the same interleaving on every run
//! (ROADMAP C3). `liveness` covers heartbeats and silent failures (C5),
//! `membership` joining, restarts and leaving (D2);
//! `random` runs seeded random scenarios against the distribution rules.
//!
//! Each targeted test fixes a frame delay, puts an event inside that window,
//! and asserts the one outcome DESIGN.md's "Distribution" rules allow.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

#[path = "../common/mod.rs"]
mod common;
mod harness;
mod liveness;
mod membership;
mod random;

use std::time::Duration;

use common::{ask, pid_text, text};
use harness::{Sim, pass, simulate};
use paddock_host::transport::memory::Faults;

/// Every frame between any two nodes takes `ms`.
fn delay(sim: &Sim, ms: u64) {
    sim.mesh.set_default_faults(Faults {
        delay: Duration::from_millis(ms),
        ..Faults::default()
    });
}

#[test]
fn monitor____target_dies_while_monitor_in_flight____one_down_noproc() {
    let (infos, reference, b) = simulate(1, 2, |sim| async move {
        delay(&sim, 10);
        let a = sim.node(1).spawn("");
        let b = sim.node(2).spawn("");
        let reference = ask(&a, &format!("monitor:{}", pid_text(b.pid()))).await;

        b.kill();
        pass(100).await;

        (ask(&a, "infos").await, reference, pid_text(b.pid()))
    });

    assert_eq!(infos, format!("down:{reference}:{b}:noproc"));
}

#[test]
fn demonitor____down_in_flight____down_dropped() {
    let infos = simulate(2, 2, |sim| async move {
        delay(&sim, 10);
        let a = sim.node(1).spawn("");
        let b = sim.node(2).spawn("");
        let reference = ask(&a, &format!("monitor:{}", pid_text(b.pid()))).await;
        pass(50).await;
        b.kill();
        pass(5).await;

        ask(&a, &format!("demonitor:{reference}")).await;
        pass(100).await;

        ask(&a, "infos").await
    });

    assert_eq!(infos, "");
}

#[test]
fn unlink____exit_in_flight____exit_dropped() {
    let infos = simulate(3, 2, |sim| async move {
        delay(&sim, 10);
        let a = sim.node(1).spawn("trap-exits");
        let b = sim.node(2).spawn("");
        let to_b = pid_text(b.pid());
        ask(&a, &format!("link:{to_b}")).await;
        pass(50).await;
        b.kill();
        pass(5).await;

        ask(&a, &format!("unlink:{to_b}")).await;
        pass(100).await;

        ask(&a, "infos").await
    });

    assert_eq!(infos, "");
}

#[test]
fn link____target_dies_while_link_in_flight____exit_noproc() {
    let (infos, b) = simulate(4, 2, |sim| async move {
        delay(&sim, 10);
        let a = sim.node(1).spawn("trap-exits");
        let b = sim.node(2).spawn("");
        ask(&a, &format!("link:{}", pid_text(b.pid()))).await;

        b.kill();
        pass(100).await;

        (ask(&a, "infos").await, pid_text(b.pid()))
    });

    assert_eq!(infos, format!("exit:{b}:noproc"));
}

#[test]
fn partition____down_in_flight____one_down_noconnection() {
    let (infos, reference, b) = simulate(5, 2, |sim| async move {
        delay(&sim, 10);
        let a = sim.node(1).spawn("");
        let b = sim.node(2).spawn("");
        let reference = ask(&a, &format!("monitor:{}", pid_text(b.pid()))).await;
        pass(50).await;
        b.kill();
        pass(5).await;

        sim.mesh.partition(1, 2);
        pass(100).await;
        sim.mesh.heal(1, 2);
        pass(100).await;

        (ask(&a, "infos").await, reference, pid_text(b.pid()))
    });

    assert_eq!(infos, format!("down:{reference}:{b}:noconnection"));
}

#[test]
fn partition____link_in_flight____initiator_gets_noconnection_target_nothing() {
    let (a_infos, b_infos, to_b) = simulate(6, 2, |sim| async move {
        delay(&sim, 10);
        let a = sim.node(1).spawn("trap-exits");
        let b = sim.node(2).spawn("trap-exits");
        ask(&a, &format!("link:{}", pid_text(b.pid()))).await;
        pass(5).await;

        sim.mesh.partition(1, 2);
        pass(5).await;
        sim.mesh.heal(1, 2);
        pass(100).await;
        let a_infos = ask(&a, "infos").await;
        // The link never reached node 2, so a's exit is no news to b.
        a.kill();
        pass(100).await;

        (a_infos, ask(&b, "infos").await, pid_text(b.pid()))
    });

    assert_eq!(a_infos, format!("exit:{to_b}:noconnection"));
    assert_eq!(b_infos, "");
}

#[test]
fn heal____before_old_frames_due____old_frames_lost() {
    let count = simulate(7, 2, |sim| async move {
        delay(&sim, 10);
        let b = sim.node(2).spawn("");
        let n1 = &sim.node(1).rt;
        n1.cast(b.pid(), text("count")).unwrap();
        pass(5).await;
        sim.mesh.partition(1, 2);
        pass(1).await;
        sim.mesh.heal(1, 2);

        n1.cast(b.pid(), text("count")).unwrap();
        pass(100).await;

        ask(&b, "count").await
    });

    // The cast sent after the heal, then this call: the first cast was lost.
    assert_eq!(count, "2");
}

#[test]
fn call____reply_after_timeout____next_call_gets_its_own_reply() {
    let (first, second) = simulate(8, 2, |sim| async move {
        delay(&sim, 15);
        let a = sim.node(1).spawn("");
        let b = sim.node(2).spawn("");
        let to_b = pid_text(b.pid());

        let first = ask(&a, &format!("call:{to_b}:20:echo:first")).await;
        let second = ask(&a, &format!("call:{to_b}:100:echo:second")).await;

        (first, second)
    });

    assert_eq!(first, "err:timeout");
    assert_eq!(second, "ok:second");
}
