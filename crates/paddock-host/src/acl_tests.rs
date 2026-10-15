#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

fn list(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

#[rstest]
#[case::both_empty(&[], &[], true)]
#[case::may_call_lists_target(&["orders"], &[], true)]
#[case::may_call_omits_target(&["billing"], &[], false)]
#[case::accept_from_lists_sender(&[], &["web"], true)]
#[case::accept_from_omits_sender(&[], &["admin"], false)]
#[case::both_list_each_other(&["orders"], &["web"], true)]
#[case::one_side_refuses(&["orders"], &["admin"], false)]
fn allowed____web_to_orders____as_both_lists_say(
    #[case] may_call: &[&str],
    #[case] accept_from: &[&str],
    #[case] expected: bool,
) {
    assert_eq!(
        allowed("web", &list(may_call), "orders", &list(accept_from)),
        expected
    );
}

#[test]
fn allowed____same_app____whatever_the_lists_say() {
    assert!(allowed(
        "orders",
        &list(&["billing"]),
        "orders",
        &list(&["web"])
    ));
}
