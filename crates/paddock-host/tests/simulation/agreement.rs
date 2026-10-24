//! Both nodes agree on every link and monitor between them:
//! one test per counterexample of spike S12's model, each failing without
//! the rule that fixes it. The fourth counterexample (a replaced QUIC
//! connection) has its test in `tests/quic.rs`.

use paddock_host::Role;
use paddock_host::transport::{Frame, Packet};

use crate::common::{ask, pid_text};
use crate::delay;
use crate::harness::{SimNode, pass, simulate};

/// S12 counterexample 1, rule R3: A links B's instance and unlinks it
/// while B links A's. Without an acknowledged unlink, B's `Link` arrives
/// after B dropped its side for A's `Unlink`, and A keeps a link B does not
/// have: a partition then tells A's instance alone.
#[test]
fn unlink____crossed_with_a_link_from_the_partner____neither_side_keeps_the_link() {
    let (a_infos, b_infos) = simulate(61, 2, |sim| async move {
        delay(&sim, 10);
        let a = sim.node(1).spawn("trap-exits");
        let b = sim.node(2).spawn("trap-exits");
        let (to_a, to_b) = (pid_text(a.pid()), pid_text(b.pid()));

        ask(&a, &format!("link:{to_b}")).await;
        ask(&a, &format!("unlink:{to_b}")).await;
        ask(&b, &format!("link:{to_a}")).await;
        pass(100).await;
        sim.mesh.partition(1, 2);
        pass(100).await;

        (ask(&a, "infos").await, ask(&b, "infos").await)
    });

    assert_eq!(a_infos, "");
    assert_eq!(b_infos, "");
}

/// S12 counterexample 2, rule R1: a partition long enough for both nodes
/// to find the other silent leaves them at the same session number. After
/// the heal, node 2 learns that node 1 hears it one heartbeat late (the
/// beat is lost, as one delayed past the next would be), so node 1 counts
/// node 2 reachable first. An instance on node 1 links an exited instance on
/// node 2, whose `noproc` answer is refused since node 2 counts node 1
/// unreachable; without R1 the linker never hears that its partner is gone.
#[test]
fn link____noproc_answer_refused_while_the_sender_is_unreachable____linker_still_told() {
    let (window, infos, to_gone) = simulate(62, 2, |sim| async move {
        let linker = sim.node(1).spawn("trap-exits");
        let gone = sim.node(2).spawn("");
        let to_gone = pid_text(gone.pid());
        gone.kill();
        let reaches = |from: u32, to: u32| sim.node(from).rt.peer_status(to).unwrap().reachable;

        sim.mesh.partition(1, 2);
        while reaches(1, 2) || reaches(2, 1) {
            pass(1).await;
        }
        sim.mesh.lose_next(1, 2, |f| {
            matches!(
                f,
                Frame::Heartbeat {
                    hears_you: true,
                    ..
                }
            )
        });
        sim.mesh.heal(1, 2);
        let mut window = false;
        for _ in 0..1000 {
            if reaches(1, 2) && !reaches(2, 1) {
                window = true;
                break;
            }
            pass(1).await;
        }
        ask(&linker, &format!("link:{to_gone}")).await;
        pass(2000).await;

        (window, ask(&linker, "infos").await, to_gone)
    });

    assert!(
        window,
        "node 1 never reached node 2 before node 2 reached it"
    );
    assert!(
        infos == format!("exit:{to_gone}:noproc")
            || infos == format!("exit:{to_gone}:noconnection"),
        "{infos:?}"
    );
}

/// S12 counterexample 3, rule R4: links and monitors with a node fail with
/// `noconnection` until a frame from its current start has been heard, and
/// work once it has.
#[test]
fn link____node_not_yet_heard____noconnection_then_links_once_heard() {
    let (before, after, to_c) = simulate(63, 2, |sim| async move {
        let a = sim.node(1).spawn("trap-exits");
        let three = SimNode::start(3, 1, Role::Member);
        sim.mesh.join(&three.rt).unwrap();
        let c = three.spawn("trap-exits");
        let to_c = pid_text(c.pid());

        ask(&a, &format!("link:{to_c}")).await;
        let before = ask(&a, "infos").await;
        while !sim.node(1).rt.peer_status(3).unwrap().contacted {
            pass(1).await;
        }
        ask(&a, &format!("link:{to_c}")).await;
        pass(50).await;
        c.kill();
        pass(50).await;

        (before, ask(&a, "infos").await, to_c)
    });

    assert_eq!(before, format!("exit:{to_c}:noconnection"));
    assert_eq!(
        after,
        format!("exit:{to_c}:noconnection;exit:{to_c}:killed")
    );
}

fn acl(accept_from: &[&str], may_call: &[&str]) -> paddock_host::acl::Acl {
    paddock_host::acl::Acl {
        accept_from: accept_from.iter().map(|a| (*a).to_string()).collect(),
        may_call: may_call.iter().map(|a| (*a).to_string()).collect(),
    }
}

/// Messaging ACLs across nodes: the target's node checks the
/// sender's stamp. A refused link leaves no side on either node.
#[test]
fn acl____across_nodes____checked_on_the_target_node() {
    let (refused, allowed, cast_count, link_infos, sides, to_orders) =
        simulate(64, 2, |sim| async move {
            let orders = sim.node(2).spawn_as("orders", "", acl(&["web"], &[]));
            let web = sim.node(1).spawn_as("web", "", acl(&[], &["orders"]));
            let admin = sim.node(1).spawn_as("admin", "trap-exits", acl(&[], &[]));
            let to = pid_text(orders.pid());

            let refused = ask(&admin, &format!("call:{to}:1000:echo:x")).await;
            let allowed = ask(&web, &format!("call:{to}:1000:echo:x")).await;
            ask(&admin, &format!("cast:{to}:count")).await;
            ask(&admin, &format!("link:{to}")).await;
            pass(50).await;
            let cast_count = ask(&orders, "count").await;
            let sides = (
                sim.node(1).rt.links(admin.pid()).links.len(),
                sim.node(2).rt.links(orders.pid()).links.len(),
            );

            (
                refused,
                allowed,
                cast_count,
                ask(&admin, "infos").await,
                sides,
                to,
            )
        });

    assert_eq!(refused, "err:access-denied");
    assert_eq!(allowed, "ok:x");
    assert_eq!(cast_count, "1");
    assert_eq!(link_infos, format!("exit:{to_orders}:noproc"));
    assert_eq!(sides, (0, 0));
}

/// A node speaks only for its own instances: a frame from node 1 whose
/// sender pid is on node 3 is dropped.
#[test]
fn frame____sender_pid_on_another_node____dropped() {
    let counts = simulate(65, 2, |sim| async move {
        let target = sim.node(2).spawn("");
        let n2 = &sim.node(2).rt;
        let session = n2.peer_status(1).unwrap().session;
        let cast = |node: u32| Packet {
            creation: 1,
            to_creation: 1,
            session,
            frame: Frame::Cast {
                from: Some(paddock_host::Pid {
                    node,
                    id: 7,
                    incarnation: 1,
                }),
                sender: Some(paddock_host::acl::Stamp {
                    app: "chaos".into(),
                    may_call: Vec::new(),
                }),
                to: target.pid(),
                msg: crate::common::text("count"),
            },
        };

        n2.deliver(1, cast(3));
        pass(10).await;
        let forged = ask(&target, "count").await;
        n2.deliver(1, cast(1));
        pass(10).await;
        let genuine = ask(&target, "count").await;
        (forged, genuine)
    });

    assert_eq!(counts, ("1".to_string(), "3".to_string()));
}
