#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn pid(node: u32, id: u64) -> Pid {
    Pid {
        node,
        id,
        incarnation: 1,
    }
}

#[test]
fn take____within_credit____debits_one_cast_and_its_bytes() {
    let credits = Credits::new(64, 1000);
    credits.add(pid(1, 1), pid(2, 1), 2, 100, 50);

    let took = credits.take(pid(1, 1), pid(2, 1), 40);

    assert!(took);
    assert_eq!(credits.held(pid(1, 1), pid(2, 1)), Some((1, 60)));
}

#[rstest::rstest]
#[case::no_credit_held(0, 0, 10)]
#[case::body_over_bytes_left(2, 30, 40)]
#[case::body_over_the_targets_largest(2, 100, 60)]
fn take____not_enough____refused_and_nothing_debited(
    #[case] messages: u32,
    #[case] bytes: u64,
    #[case] body: u64,
) {
    let credits = Credits::new(64, 1000);
    if messages > 0 {
        credits.add(pid(1, 1), pid(2, 1), messages, bytes, 50);
    }

    let took = credits.take(pid(1, 1), pid(2, 1), body);

    assert!(!took);
    if messages > 0 {
        assert_eq!(credits.held(pid(1, 1), pid(2, 1)), Some((messages, bytes)));
    }
}

#[test]
fn take____messages_used_up____refused_with_bytes_left() {
    let credits = Credits::new(64, 1000);
    credits.add(pid(1, 1), pid(2, 1), 1, 100, 50);
    credits.take(pid(1, 1), pid(2, 1), 1);

    let took = credits.take(pid(1, 1), pid(2, 1), 1);

    assert!(!took);
}

#[test]
fn untake____after_take____credit_restored() {
    let credits = Credits::new(64, 1000);
    credits.add(pid(1, 1), pid(2, 1), 2, 100, 50);
    credits.take(pid(1, 1), pid(2, 1), 40);

    credits.untake(pid(1, 1), pid(2, 1), 40);

    assert_eq!(credits.held(pid(1, 1), pid(2, 1)), Some((2, 100)));
}

#[test]
fn add____per_sender_and_target____kept_apart() {
    let credits = Credits::new(64, 1000);

    credits.add(pid(1, 1), pid(2, 1), 2, 100, 50);
    credits.add(pid(1, 2), pid(2, 1), 3, 300, 50);
    credits.add(pid(1, 1), pid(2, 1), 1, 10, 50);

    assert_eq!(credits.held(pid(1, 1), pid(2, 1)), Some((3, 110)));
    assert_eq!(credits.held(pid(1, 2), pid(2, 1)), Some((3, 300)));
    assert_eq!(credits.held(pid(1, 1), pid(2, 2)), None);
}
