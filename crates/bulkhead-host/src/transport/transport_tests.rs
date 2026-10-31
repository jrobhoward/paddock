#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn IncomingCalls____at_the_limit____refused_and_warned_once_until_half() {
    let calls = IncomingCalls::new(4);
    for _ in 0..4 {
        calls.start(1).unwrap();
    }

    let first = calls.start(1);
    let again = calls.start(1);
    let other_node = calls.start(2);
    calls.finish(1);
    let after_one = (calls.start(1), calls.start(1));
    calls.finish(1);
    calls.finish(1);
    let at_half = calls.running(1);
    let after_half = (calls.start(1), calls.start(1), calls.start(1));

    assert_eq!(first, Err(Refused::First));
    assert_eq!(again, Err(Refused::Again));
    assert_eq!(other_node, Ok(()));
    assert_eq!(after_one, (Ok(()), Err(Refused::Again)));
    assert_eq!(at_half, 2);
    assert_eq!(after_half, (Ok(()), Ok(()), Err(Refused::First)));
}

#[test]
fn IncomingCalls____no_limit____never_refused() {
    let calls = IncomingCalls::new(0);

    let results: Vec<_> = (0..1_000).map(|_| calls.start(1)).collect();

    assert!(results.iter().all(Result::is_ok));
    assert_eq!(calls.running(1), 1_000);
}

#[test]
fn IncomingCalls____all_finished____node_forgotten() {
    let calls = IncomingCalls::new(2);
    calls.start(1).unwrap();
    calls.start(1).unwrap();

    calls.finish(1);
    calls.finish(1);
    calls.finish(1);

    assert_eq!(calls.running(1), 0);
    assert!(calls.running.lock().unwrap().is_empty());
}
