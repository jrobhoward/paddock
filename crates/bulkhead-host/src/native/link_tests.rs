//! Linux and FreeBSD only: the link is.
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use rstest::rstest;

use super::{hinted_idle, too_new, turn};

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

#[test]
fn too_new____a_frame_newer_than_the_app____refused_naming_both() {
    use bulkhead_native::protocol::{FdRef, NetStream, ToApp};
    let connection = ToApp::Connection {
        listener: "l".into(),
        stream: NetStream {
            socket: 1,
            fd: FdRef(1),
            local: "127.0.0.1:1".parse().unwrap(),
            peer: "127.0.0.1:2".parse().unwrap(),
        },
    };

    let old = too_new(&connection, 10);
    let current = too_new(&connection, 11);
    let ping = too_new(&ToApp::Ping { id: 1 }, 10);

    assert_eq!(
        old.as_deref(),
        Some("the app speaks protocol version 10, which has no connection frame (version 11)")
    );
    assert_eq!(current, None);
    assert_eq!(ping, None);
}
