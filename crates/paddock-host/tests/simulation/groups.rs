//! Process groups across nodes (ROADMAP D4), as OTP's `pg`: every node sees
//! every member, a publish reaches members on every node, a member that
//! exits or a node cut off drops out, and a heal merges the groups again.

use paddock_host::acl::Acl;

use crate::common::ask;
use crate::harness::{pass, simulate};

/// A member's pids from `members:<group>`, as a count.
fn count(members: &str) -> usize {
    members.split(',').filter(|m| !m.is_empty()).count()
}

#[test]
fn groups____members_on_three_nodes____listed_everywhere_and_published_to() {
    let (listed, published, received) = simulate(81, 3, |sim| async move {
        let members: Vec<_> = (1..=3)
            .map(|n| sim.node(n).spawn_sdk("board", &["g"], Acl::default()))
            .collect();
        for m in &members {
            ask(m, "join:g").await;
        }
        let publisher = sim.node(2).spawn_sdk("feeder", &["g"], Acl::default());
        pass(300).await;

        let mut listed = Vec::new();
        for n in 1..=3 {
            let asker = sim.node(n).spawn_sdk("asker", &["g"], Acl::default());
            listed.push(count(&ask(&asker, "members:g").await));
        }
        let published = ask(&publisher, "publish:g:tick").await;
        pass(50).await;
        let mut received = Vec::new();
        for m in &members {
            received.push(ask(m, "received").await);
        }
        (listed, published, received)
    });

    assert_eq!(listed, vec![3, 3, 3]);
    assert_eq!(published, "3");
    assert_eq!(received, vec!["tick", "tick", "tick"]);
}

#[test]
fn groups____member_exits_or_node_cut_off____dropped_then_merged_on_heal() {
    let (after_exit, cut, healed) = simulate(82, 3, |sim| async move {
        let members: Vec<_> = (1..=3)
            .map(|n| sim.node(n).spawn_sdk("board", &["g"], Acl::default()))
            .collect();
        let extra = sim.node(3).spawn_sdk("board", &["g"], Acl::default());
        for m in members.iter().chain([&extra]) {
            ask(m, "join:g").await;
        }
        let asker = sim.node(1).spawn_sdk("asker", &["g"], Acl::default());
        pass(300).await;

        extra.kill();
        pass(50).await;
        let after_exit = count(&ask(&asker, "members:g").await);
        sim.mesh.partition(1, 3);
        pass(50).await;
        let cut = count(&ask(&asker, "members:g").await);
        sim.mesh.heal(1, 3);
        pass(1000).await;
        let healed = count(&ask(&asker, "members:g").await);
        (after_exit, cut, healed)
    });

    assert_eq!(after_exit, 3);
    assert_eq!(cut, 2);
    assert_eq!(healed, 3);
}

#[test]
fn groups____member_refuses_the_publisher____skipped_on_its_node() {
    let (open_got, closed_got) = simulate(83, 2, |sim| async move {
        let open = sim.node(2).spawn_sdk("board", &["g"], Acl::default());
        let closed = sim.node(2).spawn_sdk(
            "vault",
            &["g"],
            Acl {
                accept_from: vec!["auditor".into()],
                may_call: Vec::new(),
            },
        );
        ask(&open, "join:g").await;
        ask(&closed, "join:g").await;
        let publisher = sim.node(1).spawn_sdk("feeder", &["g"], Acl::default());
        pass(300).await;

        ask(&publisher, "publish:g:tick").await;
        pass(50).await;
        (ask(&open, "received").await, ask(&closed, "received").await)
    });

    assert_eq!(open_got, "tick");
    assert_eq!(closed_got, "");
}
