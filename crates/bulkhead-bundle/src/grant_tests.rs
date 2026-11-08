#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use serde_json::json;

use super::*;

fn spec(capabilities: serde_json::Value) -> AppSpec {
    AppSpec::from_json_value(&json!({ "capabilities": capabilities })).unwrap()
}

fn grants(capabilities: &[Capability]) -> Granted {
    Granted {
        capabilities: capabilities.iter().copied().collect(),
        extensions: BTreeSet::new(),
        buckets: BTreeSet::new(),
        secrets: BTreeSet::new(),
        datasources: BTreeSet::new(),
    }
}

#[test]
fn effective_grant____declared_and_granted____granted() {
    let spec = spec(json!({ "wasi:keyvalue": {} }));

    let grant = effective_grant(&spec, &grants(&[Capability::KeyValue]));

    assert_eq!(grant.granted, BTreeSet::from([Capability::KeyValue]));
    assert!(grant.denied.is_empty());
}

#[test]
fn effective_grant____declared_not_granted____denied() {
    let spec = spec(json!({ "wasi:keyvalue": {} }));

    let grant = effective_grant(&spec, &grants(&[]));

    assert_eq!(grant.denied, BTreeSet::from([Capability::KeyValue]));
    assert!(grant.granted.is_empty());
}

#[test]
fn effective_grant____granted_but_not_declared____neither() {
    let spec = spec(json!({ "wasi:logging": {} }));

    let grant = effective_grant(&spec, &grants(&[Capability::Logging, Capability::KeyValue]));

    assert_eq!(grant.granted, BTreeSet::from([Capability::Logging]));
    assert!(grant.denied.is_empty());
}

#[test]
fn EffectiveGrant____serialize____records_the_grant_for_audit() {
    let spec = spec(json!({ "wasi:keyvalue": {} }));
    let grant = effective_grant(&spec, &grants(&[]));

    let json = serde_json::to_value(&grant).unwrap();

    assert_eq!(json, json!({ "granted": [], "denied": ["wasi:keyvalue"] }));
}

#[test]
fn effective_grant____extensions____granted_or_denied_by_policy() {
    let spec = spec(json!({ "extensions": { "example:greeting": {}, "acme:other": {} } }));
    let mut operator = grants(&[]);
    operator.extensions.insert("example:greeting".into());

    let grant = effective_grant(&spec, &operator);

    assert_eq!(
        grant.granted_extensions,
        BTreeSet::from(["example:greeting".to_string()])
    );
    assert_eq!(
        grant.denied_extensions,
        BTreeSet::from(["acme:other".to_string()])
    );
}

#[test]
fn effective_grant____shared_buckets____granted_only_with_keyvalue_and_policy() {
    let spec = spec(json!({ "wasi:keyvalue": { "shared": { "points": "rw", "alarms": "ro" } } }));
    let mut with_kv = grants(&[Capability::KeyValue]);
    with_kv.buckets = BTreeSet::from(["points".to_string()]);
    let mut without_kv = grants(&[]);
    without_kv.buckets = BTreeSet::from(["points".to_string()]);

    let granted = effective_grant(&spec, &with_kv);
    let no_kv = effective_grant(&spec, &without_kv);

    assert_eq!(granted.granted_buckets, BTreeSet::from(["points".into()]));
    assert_eq!(granted.denied_buckets, BTreeSet::from(["alarms".into()]));
    assert!(no_kv.granted_buckets.is_empty());
    assert_eq!(no_kv.denied_buckets.len(), 2);
}

#[test]
fn effective_grant____http_incoming____granted_or_denied_by_policy() {
    let spec =
        spec(json!({ "wasi:http/incoming-handler": { "routes": [{ "path_prefix": "/" }] } }));

    let granted = effective_grant(&spec, &grants(&[Capability::HttpIncoming]));
    let denied = effective_grant(&spec, &grants(&[]));

    assert_eq!(granted.granted, BTreeSet::from([Capability::HttpIncoming]));
    assert_eq!(denied.denied, BTreeSet::from([Capability::HttpIncoming]));
}

#[test]
fn effective_grant____secrets____granted_only_with_capability_and_policy() {
    let spec = spec(json!({ "bulkhead:secrets": { "secrets": ["db", "api"] } }));
    let mut operator = grants(&[Capability::Secrets]);
    operator.secrets = BTreeSet::from(["db".to_string()]);
    let mut no_capability = grants(&[]);
    no_capability.secrets = BTreeSet::from(["db".to_string()]);

    let granted = effective_grant(&spec, &operator);
    let without = effective_grant(&spec, &no_capability);

    assert_eq!(granted.granted_secrets, BTreeSet::from(["db".into()]));
    assert_eq!(granted.denied_secrets, BTreeSet::from(["api".into()]));
    assert!(without.granted_secrets.is_empty());
}
