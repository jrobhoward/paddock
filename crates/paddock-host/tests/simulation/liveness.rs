//! Heartbeats, sessions and silent failures: a node that stops
//! answering is found by its peers, a one-way failure ends the session on
//! both sides, frames held across a session's end are discarded, and an
//! extra heartbeat path tells a partition from a dead node.

use paddock_host::transport::liveness::{LINK, NodeEvent};

use crate::common::{ask, pid_text, text};
use crate::harness::{drain, pass, simulate};

fn path(event: &NodeEvent) -> Option<(&str, bool)> {
    match event {
        NodeEvent::PathSilent { path, .. } => Some((path.as_str(), false)),
        NodeEvent::PathHeard { path, .. } => Some((path.as_str(), true)),
        _ => None,
    }
}

#[test]
fn stall____both_ways____noconnection_after_timeout_not_before() {
    let (early, late, b, reference, reachable) = simulate(21, 2, |sim| async move {
        let a = sim.node(1).spawn("trap-exits");
        let b = sim.node(2).spawn("");
        let to_b = pid_text(b.pid());
        ask(&a, &format!("link:{to_b}")).await;
        let reference = ask(&a, &format!("monitor:{to_b}")).await;
        pass(50).await;

        sim.mesh.stall(1, 2);
        pass(400).await;
        let early = ask(&a, "infos").await;
        pass(300).await;
        let late = ask(&a, "infos").await;
        let reachable = sim.node(1).rt.peer_status(2).unwrap().reachable;

        (early, late, to_b, reference, reachable)
    });

    assert_eq!(early, "");
    assert_eq!(
        late,
        format!("exit:{b}:noconnection;down:{reference}:{b}:noconnection")
    );
    assert!(!reachable);
}

#[test]
fn stall____one_way____both_sides_end_the_session() {
    let (a_infos, b_infos, to_a, to_b) = simulate(22, 2, |sim| async move {
        let a = sim.node(1).spawn("trap-exits");
        let b = sim.node(2).spawn("trap-exits");
        let (to_a, to_b) = (pid_text(a.pid()), pid_text(b.pid()));
        ask(&a, &format!("link:{to_b}")).await;
        pass(50).await;

        // Node 2 stops hearing node 1; node 1 still hears node 2.
        sim.mesh.stall_one_way(1, 2);
        pass(1500).await;

        (ask(&a, "infos").await, ask(&b, "infos").await, to_a, to_b)
    });

    assert_eq!(a_infos, format!("exit:{to_b}:noconnection"));
    assert_eq!(b_infos, format!("exit:{to_a}:noconnection"));
}

#[test]
fn stall____healed_before_timeout____held_frames_arrive() {
    let (infos, reference, b) = simulate(23, 2, |sim| async move {
        let a = sim.node(1).spawn("");
        let b = sim.node(2).spawn("");
        let reference = ask(&a, &format!("monitor:{}", pid_text(b.pid()))).await;
        pass(50).await;
        sim.mesh.stall(1, 2);
        b.kill();
        pass(200).await;

        sim.mesh.heal(1, 2);
        pass(1000).await;

        (ask(&a, "infos").await, reference, pid_text(b.pid()))
    });

    assert_eq!(infos, format!("down:{reference}:{b}:killed"));
}

#[test]
fn stall____healed_after_timeout____held_frames_discarded_new_links_work() {
    let (old, new, b, c, reachable) = simulate(24, 2, |sim| async move {
        let mut events = sim.node(1).rt.node_events();
        let a = sim.node(1).spawn("trap-exits");
        let b = sim.node(2).spawn("");
        let to_b = pid_text(b.pid());
        ask(&a, &format!("link:{to_b}")).await;
        pass(50).await;
        sim.mesh.stall(1, 2);
        // The link exit is held, and belongs to the session that ends.
        b.kill();
        pass(1000).await;
        sim.mesh.heal(1, 2);
        pass(500).await;
        let old = ask(&a, "infos").await;

        let c = sim.node(2).spawn("");
        let to_c = pid_text(c.pid());
        ask(&a, &format!("link:{to_c}")).await;
        pass(50).await;
        c.kill();
        pass(100).await;
        let new = ask(&a, "infos").await;
        let reachable = drain(&mut events).contains(&NodeEvent::Reachable { node: 2 });

        (old, new, to_b, to_c, reachable)
    });

    assert_eq!(old, format!("exit:{b}:noconnection"));
    assert_eq!(new, format!("{old};exit:{c}:killed"));
    assert!(reachable);
}

#[test]
fn serial____link_silent_serial_heard____a_partition_not_a_dead_node() {
    let (partition, both, status) = simulate(25, 2, |sim| async move {
        sim.mesh.add_path("serial");
        let mut events = sim.node(1).rt.node_events();
        pass(300).await;

        sim.mesh.stall(1, 2);
        pass(1000).await;
        let partition: Vec<(String, bool)> = drain(&mut events)
            .iter()
            .filter_map(path)
            .map(|(p, heard)| (p.to_string(), heard))
            .collect();
        sim.mesh.cut_path(1, 2, "serial");
        pass(1000).await;
        let both: Vec<(String, bool)> = drain(&mut events)
            .iter()
            .filter_map(path)
            .map(|(p, heard)| (p.to_string(), heard))
            .collect();

        (partition, both, sim.node(1).rt.peer_status(2).unwrap())
    });

    assert_eq!(partition, vec![(LINK.to_string(), false)]);
    assert_eq!(both, vec![("serial".to_string(), false)]);
    assert_eq!(
        status.paths,
        vec![(LINK.to_string(), false), ("serial".to_string(), false)]
    );
}

#[test]
fn liveness____runtime_dropped____its_peers_find_it_silent() {
    let events = simulate(26, 2, |mut sim| async move {
        let mut events = sim.node(1).rt.node_events();
        pass(300).await;

        drop(sim.nodes.pop());
        pass(1000).await;

        drain(&mut events)
    });

    assert!(
        events.contains(&NodeEvent::Unreachable { node: 2 }),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, NodeEvent::Down { node: 2, .. })),
        "{events:?}"
    );
}

#[test]
fn stall____healed_after_timeout____held_casts_not_delivered() {
    let count = simulate(27, 2, |sim| async move {
        let b = sim.node(2).spawn("");
        pass(50).await;
        sim.mesh.stall(1, 2);
        for _ in 0..3 {
            sim.node(1).rt.cast(b.pid(), text("count")).unwrap();
        }
        pass(1000).await;

        sim.mesh.heal(1, 2);
        pass(500).await;

        ask(&b, "count").await
    });

    // Only this call: the casts belonged to the session that ended.
    assert_eq!(count, "1");
}

#[test]
fn partition____peer_still_reachable____session_ends_once_not_each_round() {
    let (before, early, late) = simulate(28, 2, |sim| async move {
        pass(50).await;
        let session = || sim.node(1).rt.peer_status(2).unwrap().session;
        let before = session();

        // Sends fail at once; heartbeats still count the peer reachable
        // until the node timeout. The groups' snapshot, due in the new
        // session, cannot be sent meanwhile, and must not end it again.
        sim.mesh.partition(1, 2);
        pass(400).await;
        let early = session();
        let reachable = sim.node(1).rt.peer_status(2).unwrap().reachable;
        assert!(reachable, "still within the node timeout");
        pass(300).await;

        (before, early, session())
    });

    assert_eq!(early, before + 1);
    assert_eq!(
        late,
        before + 2,
        "and once more when the timeout finds it silent"
    );
}
