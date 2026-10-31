#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case(None, None, None)]
#[case(Some(50), None, Some(50))]
#[case(None, Some(1000), Some(1000))]
#[case(Some(50), Some(1000), Some(50))]
#[case(Some(5000), Some(1000), Some(1000))]
#[case(Some(0), Some(1000), Some(1000))]
fn Shared____app_and_node_rates____lower_one_applies(
    #[case] app: Option<u32>,
    #[case] node: Option<u32>,
    #[case] expected: Option<u32>,
) {
    let shared = Shared {
        node_rate: node,
        ..Shared::default()
    };

    let rate = shared.rate(app);

    assert_eq!(rate, expected);
}
