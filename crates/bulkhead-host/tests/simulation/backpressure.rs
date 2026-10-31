//! A congested link between nodes: past the message budget a
//! cast is dropped and a call fails with `overloaded`, as at a full
//! mailbox, while link and monitor notices are still let in, so congestion
//! never costs a link.

use crate::common::{ask, pid_text};
use crate::harness::{pass, simulate};

#[test]
fn congestion____casts_and_a_call_past_the_budget____dropped_overloaded_and_links_kept() {
    let (call, delivered, b_infos, to_a, sessions) = simulate(71, 2, |sim| async move {
        sim.mesh.set_send_budget(1_000);
        let a = sim.node(1).spawn("trap-exits");
        let b = sim.node(2).spawn("trap-exits");
        let (to_a, to_b) = (pid_text(a.pid()), pid_text(b.pid()));
        ask(&a, &format!("link:{to_b}")).await;
        pass(10).await;
        let before = sim.node(1).rt.peer_status(2).unwrap().session;

        // Hold node 1's frames to node 2, for less than the node timeout:
        // they count against the budget until they arrive.
        sim.mesh.stall_one_way(1, 2);
        let padding = "x".repeat(200);
        for _ in 0..30 {
            ask(&a, &format!("cast:{to_b}:count:{padding}")).await;
        }
        let call = ask(&a, &format!("call:{to_b}:100:echo:hi")).await;
        a.kill();
        pass(100).await;
        sim.mesh.heal(1, 2);
        pass(200).await;

        let delivered: u64 = ask(&b, "count").await.parse::<u64>().unwrap() - 1;
        let sessions = (before, sim.node(1).rt.peer_status(2).unwrap().session);
        (call, delivered, ask(&b, "infos").await, to_a, sessions)
    });

    assert_eq!(call, "err:overloaded");
    assert!(
        delivered > 0 && delivered < 30,
        "{delivered} casts delivered"
    );
    assert_eq!(b_infos, format!("exit:{to_a}:killed"));
    assert_eq!(sessions.0, sessions.1, "the session ended");
}
