#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::{encode, parse};

#[test]
fn encode____names____descriptors_from_eight() {
    let value = encode(["https", "http"]);

    assert_eq!(value, "https=8,http=9");
}

#[test]
fn parse____encoded____same_names_and_descriptors() {
    let found = parse(&encode(["a", "b", "c"]));

    assert_eq!(
        found,
        vec![("a".into(), 8), ("b".into(), 9), ("c".into(), 10)]
    );
}

#[test]
fn parse____descriptors_outside_the_range_or_malformed____skipped() {
    let found = parse("low=3,high=14,empty=,=9,ok=13,junk");

    assert_eq!(found, vec![("ok".into(), 13)]);
}
