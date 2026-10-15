//! Cluster membership (ROADMAP D2): joining through a seed, roles, a node
//! that restarts with a new creation, leaving, and frames from or for an
//! earlier creation.

use paddock_host::transport::liveness::NodeEvent;
use paddock_host::transport::{Frame, Packet};
use paddock_host::{Role, Runtime};

use crate::common::{ask, pid_text, text};
use crate::harness::{SimNode, drain, pass, simulate_attached};

/// Node ids and creations `rt` knows, itself included.
fn known(rt: &Runtime) -> Vec<(u32, u32)> {
    rt.members().iter().map(|m| (m.node, m.creation)).collect()
}

#[test]
fn join____through_one_seed____every_node_knows_every_member() {
    let (views, roles, reachable, unreachable) = simulate_attached(
        31,
        &[Role::Member, Role::Member, Role::Witness],
        |sim| async move {
            let mut events = sim.node(1).rt.node_events();
            sim.node(2).rt.join_cluster(&[1]);
            pass(50).await;

            sim.node(3).rt.join_cluster(&[1]);
            pass(2000).await;

            let views: Vec<Vec<(u32, u32)>> = sim.nodes.iter().map(|n| known(&n.rt)).collect();
            let roles: Vec<Role> = sim.node(2).rt.members().iter().map(|m| m.role).collect();
            let reachable = [(2, 3), (3, 2)]
                .iter()
                .all(|&(x, y)| sim.node(x).rt.peer_status(y).is_some_and(|p| p.reachable));
            let unreachable = drain(&mut events)
                .into_iter()
                .any(|e| matches!(e, NodeEvent::Unreachable { .. }));
            (views, roles, reachable, unreachable)
        },
    );

    let all = vec![(1, 1), (2, 1), (3, 1)];
    assert_eq!(views, vec![all.clone(), all.clone(), all]);
    assert_eq!(roles, vec![Role::Member, Role::Member, Role::Witness]);
    assert!(
        reachable,
        "2 and 3 heartbeat each other, though neither was a seed"
    );
    assert!(!unreachable);
}

#[test]
fn restart____higher_creation____old_session_ends_new_one_works() {
    let (infos, b, events, view, call) =
        simulate_attached(32, &[Role::Member, Role::Member], |mut sim| async move {
            sim.node(2).rt.join_cluster(&[1]);
            pass(50).await;
            let mut events = sim.node(1).rt.node_events();
            let a = sim.node(1).spawn("trap-exits");
            let b = sim.node(2).spawn("");
            let to_b = pid_text(b.pid());
            ask(&a, &format!("link:{to_b}")).await;
            pass(50).await;

            // Node 2 goes silent and dies before node 1 notices; its
            // instance's exit never leaves.
            sim.mesh.stall(1, 2);
            b.kill();
            drop(b);
            pass(10).await;
            drop(sim.nodes.pop());
            sim.mesh.heal(1, 2);
            let restarted = SimNode::start(2, 2, Role::Member);
            sim.mesh.attach(&restarted.rt).unwrap();
            restarted.rt.join_cluster(&[1]);
            sim.nodes.push(restarted);
            pass(100).await;

            let c = sim.node(2).spawn("");
            let call = ask(&a, &format!("call:{}:1000:echo:again", pid_text(c.pid()))).await;
            (
                ask(&a, "infos").await,
                to_b,
                drain(&mut events),
                known(&sim.node(1).rt),
                call,
            )
        });

    assert_eq!(infos, format!("exit:{b}:noconnection"));
    assert!(
        events.contains(&NodeEvent::Joined {
            node: 2,
            creation: 2
        }),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, NodeEvent::Down { node: 2, .. })),
        "{events:?}"
    );
    assert_eq!(view, vec![(1, 1), (2, 2)]);
    assert_eq!(call, "ok:again");
}

#[test]
fn leave____members_told____sessions_end_and_heartbeats_stop() {
    let (infos, c, view, events) = simulate_attached(
        33,
        &[Role::Member, Role::Member, Role::Member],
        |sim| async move {
            sim.node(2).rt.join_cluster(&[1]);
            sim.node(3).rt.join_cluster(&[1]);
            pass(200).await;
            let a = sim.node(1).spawn("trap-exits");
            let c = sim.node(3).spawn("");
            let to_c = pid_text(c.pid());
            ask(&a, &format!("link:{to_c}")).await;
            pass(50).await;
            let mut events = sim.node(1).rt.node_events();

            sim.node(3).rt.leave_cluster();
            pass(2000).await;

            (
                ask(&a, "infos").await,
                to_c,
                known(&sim.node(1).rt),
                drain(&mut events),
            )
        },
    );

    assert_eq!(infos, format!("exit:{c}:noconnection"));
    assert_eq!(view, vec![(1, 1), (2, 1)]);
    assert!(events.contains(&NodeEvent::Left { node: 3 }), "{events:?}");
    assert!(
        !events.contains(&NodeEvent::Unreachable { node: 3 }),
        "a node that left is not watched: {events:?}"
    );
}

#[test]
fn packet____for_or_from_an_earlier_creation____discarded() {
    let counts = simulate_attached(34, &[Role::Member, Role::Member], |sim| async move {
        sim.node(2).rt.join_cluster(&[1]);
        pass(200).await;
        let b = sim.node(2).spawn("");
        let n2 = &sim.node(2).rt;
        let cast = |creation, to_creation| Packet {
            creation,
            to_creation,
            session: 0,
            frame: Frame::Cast {
                from: None,
                sender: None,
                to: b.pid(),
                msg: text("count"),
            },
        };
        let mut counts = Vec::new();

        // From node 1's current creation, to node 2's: delivered.
        n2.deliver(1, cast(1, 1));
        counts.push(ask(&b, "count").await);
        // Meant for an earlier start of node 2.
        n2.deliver(1, cast(1, 7));
        counts.push(ask(&b, "count").await);
        // Node 1 restarted (creation 2); then its creation 1 is stale.
        n2.deliver(1, cast(2, 1));
        n2.deliver(1, cast(1, 1));
        counts.push(ask(&b, "count").await);

        counts
    });

    // Each `count` call counts itself too.
    assert_eq!(counts, vec!["2", "3", "5"]);
}
