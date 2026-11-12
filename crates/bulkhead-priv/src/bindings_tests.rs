#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

const RANGE: RangeInclusive<u32> = 60000..=60002;

#[test]
fn assign____new_apps____lowest_free_user_each() {
    let mut b = Bindings::default();

    let first = b.assign("a", &RANGE).unwrap();
    let second = b.assign("b", &RANGE).unwrap();
    let again = b.assign("a", &RANGE).unwrap();

    assert_eq!(first, (60000, true));
    assert_eq!(second, (60001, true));
    assert_eq!(again, (60000, false));
}

#[test]
fn assign____range_full____refused() {
    let mut b = Bindings::default();
    for app in ["a", "b", "c"] {
        b.assign(app, &RANGE).unwrap();
    }

    let err = b.assign("d", &RANGE).unwrap_err();

    assert!(err.contains("no user left"), "{err}");
}

#[test]
fn assign____user_outside_a_shrunk_range____refused_not_moved() {
    let mut b = Bindings::parse("60005 a\n").unwrap();

    let err = b.assign("a", &RANGE).unwrap_err();

    assert!(err.contains("outside"), "{err}");
    assert_eq!(b.get("a"), Some(60005));
}

#[test]
fn render____then_parse____the_same() {
    let mut b = Bindings::default();
    b.assign("orders", &RANGE).unwrap();
    b.assign("hmi", &RANGE).unwrap();

    let back = Bindings::parse(&b.render()).unwrap();

    assert_eq!(back, b);
}

#[test]
fn parse____a_user_twice____refused() {
    assert!(Bindings::parse("60000 a\n60000 b\n").is_err());
    assert!(Bindings::parse("60000 a\n60001 a\n").is_err());
    assert!(Bindings::parse("60000 ../x\n").is_err());
    assert!(Bindings::parse("x a\n").is_err());
}

#[test]
fn save____then_load____the_same() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = Bindings::default();
    b.assign("orders", &RANGE).unwrap();

    b.save(dir.path()).unwrap();
    let back = Bindings::load(dir.path()).unwrap();

    assert_eq!(back, b);
}

#[test]
fn load____no_record____empty() {
    let dir = tempfile::tempdir().unwrap();

    assert_eq!(Bindings::load(dir.path()).unwrap(), Bindings::default());
}
