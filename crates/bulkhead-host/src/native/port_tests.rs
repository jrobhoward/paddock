//! Linux only: the port is.
#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use rstest::rstest;

use super::linux::{hinted_idle, turn};

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

#[rstest]
#[case::idle(true, 100, true)]
#[case::called_back_to_back(true, 5, false)]
#[case::more_queued(false, 100, false)]
fn hinted_idle____queue_and_gap____idle_only_when_both_say_so(
    #[case] nothing_queued: bool,
    #[case] since_us: u64,
    #[case] idle: bool,
) {
    let spin = Duration::from_micros(20);

    let hinted = hinted_idle(nothing_queued, Duration::from_micros(since_us), spin);

    assert_eq!(hinted, idle);
}
