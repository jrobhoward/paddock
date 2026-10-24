#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::{MAX_WAITS, Waits, bindings::paddock::io::ready};

#[test]
fn notify____same_pollable_twice____tag_replaced() {
    let mut waits = Waits::default();

    waits.notify(7, 1).unwrap();
    waits.notify(7, 2).unwrap();

    assert_eq!(waits.waiting, vec![(7, 2)]);
}

#[test]
fn notify____past_the_limit____limit_reached() {
    let mut waits = Waits::default();
    for rep in 0..MAX_WAITS {
        waits.notify(u32::try_from(rep).unwrap(), 0).unwrap();
    }

    let over = waits.notify(u32::MAX, 0);

    assert!(matches!(over, Err(ready::Error::LimitReached)));
}

#[test]
fn forget____waited_or_not____whether_it_was() {
    let mut waits = Waits::default();
    waits.notify(3, 9).unwrap();

    let first = waits.forget(3);
    let second = waits.forget(3);

    assert!(first && !second);
    assert!(waits.waiting.is_empty());
}
