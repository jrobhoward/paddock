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
