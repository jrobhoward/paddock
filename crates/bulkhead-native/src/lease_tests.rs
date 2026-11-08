#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case::none(&[], None)]
#[case::partial(&[1, 0, 0], None)]
#[case::one(&5u64.to_le_bytes(), Some(5))]
#[case::several(&[7u64.to_le_bytes(), 9u64.to_le_bytes()].concat(), Some(9))]
fn last_deadline____pipe_bytes____the_last_whole_one(
    #[case] bytes: &[u8],
    #[case] found: Option<u64>,
) {
    assert_eq!(last_deadline(bytes), found);
}

#[test]
fn now_ns____twice____does_not_go_back() {
    let a = now_ns();
    let b = now_ns();

    assert!(a > 0 && b >= a);
}
