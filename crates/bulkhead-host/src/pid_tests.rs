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
#[case("2.5.1@3")]
#[case("<2.5.1@3>")]
fn Pid____parse_with_creation____id_holds_both(#[case] text: &str) {
    let pid: Pid = text.parse().unwrap();

    assert_eq!(pid.id, (3 << 40) | 5);
    assert_eq!(
        (pid.node, pid.serial(), pid.creation(), pid.incarnation),
        (2, 5, 3, 1)
    );
}

#[rstest]
#[case("")]
#[case("0.12")]
#[case("0.12.3@")]
#[case("0.12.3@x")]
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
        id: instance_id(4, 99),
        incarnation: 2,
    };

    let shown = pid.to_string();
    let parsed: Pid = shown.parse().unwrap();

    assert_eq!(shown, "<7.99.2@4>");
    assert_eq!(parsed, pid);
}
