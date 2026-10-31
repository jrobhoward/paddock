#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn outcome____ok_body____healthy() {
    let reply = Envelope::new("text/plain", "ok");

    assert_eq!(outcome(&Ok(reply)), Ok(()));
}

#[test]
fn outcome____other_body_or_an_error____why_not() {
    let reply = Envelope::new("text/plain", "database unreachable");

    assert_eq!(outcome(&Ok(reply)), Err("database unreachable".to_string()));
    assert!(outcome(&Err(CallError::Timeout)).is_err());
}

#[test]
fn streak____failures_in_a_row____kills_at_the_threshold() {
    let mut streak = Streak::new(3);

    let kills: Vec<bool> = [false, false, true, false, false, false]
        .into_iter()
        .map(|healthy| streak.record(healthy))
        .collect();

    assert_eq!(kills, vec![false, false, false, false, false, true]);
}
