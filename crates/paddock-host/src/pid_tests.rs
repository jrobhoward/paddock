#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case("0.12.3")]
#[case("<0.12.3>")]
#[case(" 0.12.3 ")]
fn Pid____parse_valid____reads_fields(#[case] text: &str) {
    let pid: Pid = text.parse().unwrap();

    assert_eq!(
        pid,
        Pid {
            node: 0,
            id: 12,
            incarnation: 3
        }
    );
}

#[rstest]
#[case("")]
#[case("0.12")]
#[case("0.12.3.4")]
#[case("a.b.c")]
#[case("0.-1.3")]
fn Pid____parse_invalid____error(#[case] text: &str) {
    assert!(text.parse::<Pid>().is_err());
}

#[test]
fn Pid____display_then_parse____round_trips() {
    let pid = Pid {
        node: 7,
        id: 99,
        incarnation: 2,
    };

    let parsed: Pid = pid.to_string().parse().unwrap();

    assert_eq!(parsed, pid);
}
