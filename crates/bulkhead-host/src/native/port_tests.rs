//! Linux only: the port is.
#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::linux::turn;

#[test]
fn turn____nothing_between____one_count_apart() {
    let before = turn();

    let after = turn();

    assert_eq!(after, (before.0, before.1 + 1));
}

#[test]
fn turn____another_turn_between____more_than_one_apart() {
    let before = turn();

    turn();
    let after = turn();

    assert_ne!(after, (before.0, before.1 + 1));
}

#[test]
fn turn____another_thread____other_counter() {
    let here = turn();

    let there = std::thread::spawn(turn).join().unwrap();

    assert_ne!(there.0, here.0);
}
