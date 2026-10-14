#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn pid(id: u64) -> Pid {
    Pid {
        node: 0,
        id,
        incarnation: 1,
    }
}

fn alive(_: Pid) -> bool {
    true
}

fn dead(_: Pid) -> bool {
    false
}

#[test]
fn link____both_alive____each_side_hears_the_others_exit() {
    let table = LinkTable::default();
    table.link(pid(1), pid(2), alive);

    let notices = table.on_exit(pid(2));

    assert_eq!(notices.linked, vec![pid(1)]);
    assert_eq!(table.on_exit(pid(1)), ExitNotices::default());
}

#[test]
fn link____target_dead____refused() {
    let table = LinkTable::default();

    let linked = table.link(pid(1), pid(2), dead);

    assert!(!linked);
    assert_eq!(table.on_exit(pid(1)), ExitNotices::default());
}

#[test]
fn unlink____after_link____exit_not_reported() {
    let table = LinkTable::default();
    table.link(pid(1), pid(2), alive);

    table.unlink(pid(2), pid(1));

    assert_eq!(table.on_exit(pid(1)), ExitNotices::default());
    assert_eq!(table.on_exit(pid(2)), ExitNotices::default());
}

#[test]
fn monitor____target_exits____watcher_listed_with_reference() {
    let table = LinkTable::default();
    let (reference, was_alive) = table.monitor(pid(1), pid(2), alive);

    let notices = table.on_exit(pid(2));

    assert!(was_alive);
    assert_eq!(notices.monitors, vec![(reference, pid(1))]);
}

#[test]
fn monitor____twice____two_distinct_references() {
    let table = LinkTable::default();

    let (a, _) = table.monitor(pid(1), pid(2), alive);
    let (b, _) = table.monitor(pid(1), pid(2), alive);

    assert_ne!(a, b);
    assert_eq!(table.on_exit(pid(2)).monitors.len(), 2);
}

#[test]
fn monitor____target_dead____reported_dead_and_not_stored() {
    let table = LinkTable::default();

    let (_, was_alive) = table.monitor(pid(1), pid(2), dead);

    assert!(!was_alive);
    assert_eq!(table.on_exit(pid(2)), ExitNotices::default());
}

#[test]
fn demonitor____before_exit____no_notice() {
    let table = LinkTable::default();
    let (reference, _) = table.monitor(pid(1), pid(2), alive);

    table.demonitor(pid(1), reference);

    assert_eq!(table.on_exit(pid(2)), ExitNotices::default());
}

#[test]
fn demonitor____by_another_watcher____ignored() {
    let table = LinkTable::default();
    let (reference, _) = table.monitor(pid(1), pid(2), alive);

    table.demonitor(pid(3), reference);

    assert_eq!(table.on_exit(pid(2)).monitors, vec![(reference, pid(1))]);
}

#[test]
fn on_exit____watcher_exits_first____its_monitors_are_dropped() {
    let table = LinkTable::default();
    table.monitor(pid(1), pid(2), alive);

    table.on_exit(pid(1));

    assert_eq!(table.on_exit(pid(2)), ExitNotices::default());
}

fn remote(id: u64) -> Pid {
    Pid {
        node: 9,
        id,
        incarnation: 1,
    }
}

#[test]
fn unlink_remote____link_from_the_partner_before_the_ack____ignored() {
    let table = LinkTable::default();
    table.link(pid(1), remote(2), alive);
    let id = table.unlink_remote(pid(1), remote(2)).unwrap();

    let crossed = table.link_from(pid(1), remote(2), alive);
    table.unlinked(pid(1), remote(2), id);
    let after_ack = table.link_from(pid(1), remote(2), alive);

    assert_eq!(crossed, LinkFrom::Unlinking);
    assert_eq!(after_ack, LinkFrom::Linked);
}

#[test]
fn unlink_remote____not_linked____nothing_to_send() {
    let table = LinkTable::default();

    let id = table.unlink_remote(pid(1), remote(2));

    assert_eq!(id, None);
    assert_eq!(table.unlinking(), 0);
}

#[test]
fn unlinked____ack_of_an_older_unlink____keeps_the_newer_one() {
    let table = LinkTable::default();
    table.link(pid(1), remote(2), alive);
    let first = table.unlink_remote(pid(1), remote(2)).unwrap();
    table.link(pid(1), remote(2), alive);
    let second = table.unlink_remote(pid(1), remote(2)).unwrap();

    table.unlinked(pid(1), remote(2), first);

    assert_ne!(first, second);
    assert_eq!(
        table.link_from(pid(1), remote(2), alive),
        LinkFrom::Unlinking
    );
}

#[test]
fn link_from____local_dead____dead() {
    let table = LinkTable::default();

    let result = table.link_from(pid(1), remote(2), dead);

    assert_eq!(result, LinkFrom::Dead);
    assert_eq!(table.on_exit(pid(1)), ExitNotices::default());
}

#[test]
fn unlinking____local_exits_or_node_lost____forgotten() {
    let table = LinkTable::default();
    table.link(pid(1), remote(2), alive);
    table.link(pid(3), remote(4), alive);
    table.unlink_remote(pid(1), remote(2)).unwrap();
    table.unlink_remote(pid(3), remote(4)).unwrap();

    table.on_exit(pid(1));
    table.on_node_down(9);

    assert_eq!(table.unlinking(), 0);
}
