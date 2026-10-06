#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

const POLICY: &str = r#"
[defaults]
grant = ["wasi:logging"]

[apps.orders]
grant = ["wasi:keyvalue"]

[signers.payments]
key = "RWQpayments"
grant = ["wasi:http/outgoing-handler"]
"#;

#[test]
fn OperatorPolicy____granted____unions_defaults_app_and_signer() {
    let policy = OperatorPolicy::from_toml_str(POLICY).unwrap();

    let granted = policy.granted("orders", "RWQpayments");

    assert_eq!(
        granted,
        BTreeSet::from([
            Capability::Logging,
            Capability::KeyValue,
            Capability::HttpOutgoing
        ])
    );
}

#[test]
fn OperatorPolicy____other_app_and_signer____gets_defaults_only() {
    let policy = OperatorPolicy::from_toml_str(POLICY).unwrap();

    let granted = policy.granted("billing", "RWQother");

    assert_eq!(granted, BTreeSet::from([Capability::Logging]));
}

#[test]
fn OperatorPolicy____empty_file____grants_nothing() {
    let policy = OperatorPolicy::from_toml_str("").unwrap();

    let granted = policy.granted("orders", "RWQ");

    assert!(granted.is_empty());
}

#[test]
fn OperatorPolicy____unknown_capability____returns_policy_error() {
    let result = OperatorPolicy::from_toml_str("[defaults]\ngrant = [\"wasi:telepathy\"]\n");

    let err = result.unwrap_err().to_string();

    assert!(err.contains("unknown capability 'wasi:telepathy'"), "{err}");
}

#[test]
fn OperatorPolicy____unknown_field____returns_policy_error() {
    let result = OperatorPolicy::from_toml_str("[defaults]\ngrants = []\n");

    assert!(matches!(result, Err(BundleError::Policy(_))));
}

#[test]
fn OperatorPolicy____missing_file____returns_policy_error() {
    let result = OperatorPolicy::from_file("/nonexistent/paddock-policy.toml");

    assert!(matches!(result, Err(BundleError::Policy(_))));
}
