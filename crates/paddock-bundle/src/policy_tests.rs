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
        granted.capabilities,
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

    assert_eq!(granted.capabilities, BTreeSet::from([Capability::Logging]));
}

#[test]
fn OperatorPolicy____buckets____union_of_matching_entries() {
    let policy = OperatorPolicy::from_toml_str(
        "[defaults]\nbuckets = [\"status\"]\n[apps.scada]\nbuckets = [\"points\"]\n\
         [signers.ops]\nkey = \"RWQops\"\nbuckets = [\"alarms\"]\n",
    )
    .unwrap();

    let scada = policy.granted("scada", "RWQops");
    let other = policy.granted("web", "RWQother");

    assert_eq!(
        scada.buckets,
        BTreeSet::from(["alarms".into(), "points".into(), "status".into()])
    );
    assert_eq!(other.buckets, BTreeSet::from(["status".into()]));
}

#[test]
fn OperatorPolicy____empty_file____grants_nothing() {
    let policy = OperatorPolicy::from_toml_str("").unwrap();

    let granted = policy.granted("orders", "RWQ");

    assert_eq!(granted, Granted::default());
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

#[test]
fn OperatorPolicy____extension_in_grant____granted_as_extension() {
    let policy = OperatorPolicy::from_toml_str(
        "[defaults]\ngrant = [\"wasi:logging\", \"example:greeting\"]\n",
    )
    .unwrap();

    let granted = policy.granted("any", "RWQany");

    assert_eq!(granted.capabilities, BTreeSet::from([Capability::Logging]));
    assert_eq!(
        granted.extensions,
        BTreeSet::from(["example:greeting".to_string()])
    );
}

#[test]
fn OperatorPolicy____reserved_or_bad_key____refused() {
    let reserved = OperatorPolicy::from_toml_str("[defaults]\ngrant = [\"wasi:telepathy\"]\n");
    let bad = OperatorPolicy::from_toml_str("[defaults]\ngrant = [\"nonsense\"]\n");

    assert!(reserved.is_err());
    assert!(bad.is_err());
}

#[test]
fn OperatorPolicy____secrets____union_of_matching_entries() {
    let policy = OperatorPolicy::from_toml_str(
        "[apps.billing]\nsecrets = [\"stripe\"]\n[signers.ops]\nkey = \"RWQops\"\nsecrets = [\"db\"]\n",
    )
    .unwrap();

    let billing = policy.granted("billing", "RWQops");
    let other = policy.granted("web", "RWQother");

    assert_eq!(
        billing.secrets,
        BTreeSet::from(["db".into(), "stripe".into()])
    );
    assert!(other.secrets.is_empty());
}

#[test]
fn OperatorPolicy____datasources____union_of_matching_entries() {
    let policy = OperatorPolicy::from_toml_str(
        "[defaults]\ndatasources = [\"reports\"]\n[apps.billing]\ndatasources = [\"main\"]\n",
    )
    .unwrap();

    let billing = policy.granted("billing", "RWQother");
    let other = policy.granted("other", "RWQother");

    assert_eq!(
        billing.datasources,
        ["main", "reports"].map(String::from).into()
    );
    assert_eq!(other.datasources, ["reports"].map(String::from).into());
}
