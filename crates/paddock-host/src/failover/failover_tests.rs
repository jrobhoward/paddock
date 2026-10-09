#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case(Standing::Primary, false, 7)]
#[case(Standing::Standby, true, u64::MAX)]
#[case(Standing::Down, false, 0)]
fn Status____encode_then_decode____round_trips(
    #[case] standing: Standing,
    #[case] promoting: bool,
    #[case] epoch: u64,
) {
    let status = Status {
        standing,
        promoting,
        epoch,
        stamp: epoch.wrapping_add(1),
        echo: 42,
    };

    assert_eq!(Status::decode(&status.encode()), Some(status));
}

#[rstest]
#[case(&[])]
#[case(&[1, 0, 1])]
#[case(&[9, 0, 0, 0, 0, 0, 0, 0, 0, 0])]
#[case(&[1; 25])]
#[case(&[1; 27])]
fn Status____malformed____none(#[case] bytes: &[u8]) {
    assert_eq!(Status::decode(bytes), None);
}
