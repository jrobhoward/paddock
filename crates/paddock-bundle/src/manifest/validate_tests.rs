#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case("orders")]
#[case("a")]
#[case("billing-v2")]
#[case("kv_cache")]
#[case("9lives")]
fn is_valid_name____valid____returns_true(#[case] name: &str) {
    assert!(is_valid_name(name));
}

#[rstest]
#[case("")]
#[case("Orders")]
#[case("-orders")]
#[case("_orders")]
#[case("or ders")]
#[case("orders.v2")]
#[case("ördërs")]
fn is_valid_name____invalid____returns_false(#[case] name: &str) {
    assert!(!is_valid_name(name));
}

#[test]
fn is_valid_name____over_max_length____returns_false() {
    let name = "a".repeat(MAX_NAME_LEN + 1);

    let valid = is_valid_name(&name);

    assert!(!valid);
}

#[test]
fn Validator____nested_error____reports_dotted_path() {
    let mut v = Validator::default();

    v.at("limits", |v| {
        v.error_at("memory_max", "must be greater than zero")
    });
    let err = v.finish().unwrap_err();

    assert_eq!(
        err.to_string(),
        "invalid manifest: limits.memory_max: must be greater than zero"
    );
}

#[test]
fn Validator____unique_names_with_duplicate____reports_index() {
    let mut v = Validator::default();

    v.unique_names("registered_names", ["orders", "orders"]);
    let ManifestError::Invalid(errors) = v.finish().unwrap_err() else {
        panic!("expected Invalid");
    };

    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].path, "registered_names[1]");
}

#[test]
fn Validator____no_errors____finishes_ok() {
    let v = Validator::default();

    let result = v.finish();

    assert!(result.is_ok());
}
