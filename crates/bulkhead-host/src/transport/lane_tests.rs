#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn admit____messages_past_the_budget____busy_until_written() {
    let lane = Lane::new(100, 1000);
    let first = lane.admit(Class::Message, 60).unwrap();
    let second = lane.admit(Class::Message, 60).unwrap();

    let refused = lane.admit(Class::Message, 1);
    lane.done(first);
    let after = lane.admit(Class::Message, 1);

    assert_eq!(refused, Err(SendError::Busy));
    assert!(after.is_ok());
    lane.done(second);
    assert_eq!(lane.stats().refused, 1);
}

#[test]
fn admit____notice_with_messages_over_budget____let_in() {
    let lane = Lane::new(10, 1000);
    lane.admit(Class::Message, 50).unwrap();

    let notice = lane.admit(Class::Notice, 30);

    assert!(notice.is_ok());
    assert_eq!(lane.stats().queued_bytes, 50);
    assert_eq!(lane.stats().queued_frames, 2);
}

#[test]
fn admit____message_larger_than_the_budget_into_an_empty_lane____let_in() {
    let lane = Lane::new(10, 1000);

    let big = lane.admit(Class::Message, 1_000_000);

    assert!(big.is_ok());
}

#[test]
fn admit____frame_cap_reached____notices_and_messages_full() {
    let lane = Lane::new(u64::MAX, 2);
    lane.admit(Class::Notice, 1).unwrap();
    lane.admit(Class::Notice, 1).unwrap();

    assert_eq!(lane.admit(Class::Notice, 1), Err(SendError::Full));
    assert_eq!(lane.admit(Class::Message, 1), Err(SendError::Full));
}

#[test]
fn class____messages_and_notices____told_apart() {
    use crate::message::{Envelope, ExitReason};
    use crate::pid::Pid;
    let p = Pid {
        node: 1,
        id: 1,
        incarnation: 1,
    };

    let cast = Frame::Cast {
        from: None,
        sender: None,
        to: p,
        msg: Envelope::new("text/plain", "x"),
    };
    let exit = Frame::LinkExit {
        from: p,
        to: p,
        reason: ExitReason::Normal,
    };

    assert_eq!(Class::of(&cast), Class::Message);
    assert_eq!(Class::of(&exit), Class::Notice);
}
