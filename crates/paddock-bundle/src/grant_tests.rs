#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use serde_json::json;

use super::*;

const PROCESS: &str = "paddock:runtime/process@0.1.0";
const KV: &str = "wasi:keyvalue/store@0.2.0-draft";
const LOG: &str = "wasi:logging/logging@0.1.0-draft";
const HTTP_OUT: &str = "wasi:http/outgoing-handler@0.2.12";
const HTTP_TYPES: &str = "wasi:http/types@0.2.12";

fn spec(capabilities: serde_json::Value) -> AppSpec {
    AppSpec::from_json_value(&json!({ "capabilities": capabilities })).unwrap()
}

fn imports(names: &[&str]) -> Vec<String> {
    names.iter().map(ToString::to_string).collect()
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
fn effective_grant____declared_and_granted____links_host_implementation() {
    let spec = spec(json!({ "wasi:keyvalue": {} }));

    let grant = effective_grant(
        &imports(&[PROCESS, KV]),
        &spec,
        &grants(&[Capability::KeyValue]),
    )
    .unwrap();

    assert_eq!(grant.imports[PROCESS], ImportLink::Implicit);
    assert_eq!(grant.imports[KV], ImportLink::Granted(Capability::KeyValue));
    assert_eq!(grant.granted, BTreeSet::from([Capability::KeyValue]));
    assert!(grant.denied.is_empty());
    assert!(grant.unused.is_empty());
}

#[test]
fn effective_grant____declared_not_granted____links_deny_stubs() {
    let spec = spec(json!({ "wasi:keyvalue": {} }));

    let grant = effective_grant(&imports(&[KV]), &spec, &grants(&[])).unwrap();

    assert_eq!(grant.imports[KV], ImportLink::Denied(Capability::KeyValue));
    assert_eq!(grant.denied, BTreeSet::from([Capability::KeyValue]));
    assert!(grant.granted.is_empty());
}

#[test]
fn effective_grant____declared_not_imported____reported_unused() {
    let spec = spec(json!({ "wasi:keyvalue": {}, "wasi:logging": {} }));

    let grant = effective_grant(
        &imports(&[LOG]),
        &spec,
        &grants(&[Capability::Logging, Capability::KeyValue]),
    )
    .unwrap();

    assert_eq!(grant.granted, BTreeSet::from([Capability::Logging]));
    assert_eq!(grant.unused, BTreeSet::from([Capability::KeyValue]));
}

#[test]
fn effective_grant____undeclared_and_unknown_imports____refuses_listing_all() {
    let spec = spec(json!({}));

    let err = effective_grant(
        &imports(&[KV, "paddock:spike4/kv@0.1.0", LOG]),
        &spec,
        &grants(&[]),
    )
    .unwrap_err();

    let BundleError::ImportsRefused {
        undeclared,
        unknown,
    } = err
    else {
        panic!("expected ImportsRefused, got {err:?}");
    };
    assert_eq!(
        undeclared,
        vec![
            format!("{KV} (needs wasi:keyvalue)"),
            format!("{LOG} (needs wasi:logging)"),
        ]
    );
    assert_eq!(unknown, vec!["paddock:spike4/kv@0.1.0"]);
}

#[test]
fn effective_grant____http_types_with_outgoing_declared____follows_outgoing() {
    let spec = spec(json!({ "wasi:http/outgoing-handler": { "allow": ["https://x.internal"] } }));

    let grant = effective_grant(
        &imports(&[HTTP_OUT, HTTP_TYPES]),
        &spec,
        &grants(&[Capability::HttpOutgoing]),
    )
    .unwrap();

    assert_eq!(
        grant.imports[HTTP_TYPES],
        ImportLink::Granted(Capability::HttpOutgoing)
    );
}

#[test]
fn effective_grant____http_types_with_nothing_declared____refused() {
    let spec = spec(json!({}));

    let err = effective_grant(&imports(&[HTTP_TYPES]), &spec, &grants(&[])).unwrap_err();

    assert!(
        err.to_string().contains(
            "wasi:http/types@0.2.12 (needs wasi:http/outgoing-handler or wasi:http/incoming-handler)"
        ),
        "{err}"
    );
}

#[test]
fn EffectiveGrant____serialize____records_links_for_audit() {
    let spec = spec(json!({ "wasi:keyvalue": {} }));
    let grant = effective_grant(&imports(&[PROCESS, KV]), &spec, &grants(&[])).unwrap();

    let json = serde_json::to_value(&grant).unwrap();

    assert_eq!(
        json,
        json!({
            "imports": {
                PROCESS: { "link": "implicit" },
                KV: { "link": "denied", "capability": "wasi:keyvalue" }
            },
            "granted": [],
            "denied": ["wasi:keyvalue"],
            "unused": []
        })
    );
}

const GREET: &str = "example:greeting/greet@0.1.0";

#[test]
fn effective_grant____extension_declared_and_granted____linked_to_service() {
    let spec = spec(json!({ "extensions": { "example:greeting": { "salutation": "Hi" } } }));
    let operator = Granted {
        capabilities: BTreeSet::new(),
        extensions: BTreeSet::from(["example:greeting".to_string()]),
        buckets: BTreeSet::new(),
        secrets: BTreeSet::new(),
        datasources: BTreeSet::new(),
    };

    let grant = effective_grant(&imports(&[PROCESS, GREET]), &spec, &operator).unwrap();

    assert_eq!(
        grant.imports[GREET],
        ImportLink::GrantedExtension("example:greeting".into())
    );
    assert_eq!(
        grant.granted_extensions,
        BTreeSet::from(["example:greeting".to_string()])
    );
}

#[test]
fn effective_grant____extension_declared_not_granted____deny_stubs() {
    let spec = spec(json!({ "extensions": { "example:greeting": {} } }));

    let grant = effective_grant(&imports(&[GREET]), &spec, &grants(&[])).unwrap();

    assert_eq!(
        grant.imports[GREET],
        ImportLink::DeniedExtension("example:greeting".into())
    );
    assert_eq!(
        grant.denied_extensions,
        BTreeSet::from(["example:greeting".to_string()])
    );
}

#[test]
fn effective_grant____extension_not_declared____refused() {
    let err = effective_grant(&imports(&[GREET]), &spec(json!({})), &grants(&[])).unwrap_err();

    let BundleError::ImportsRefused { undeclared, .. } = err else {
        panic!("expected ImportsRefused, got {err:?}");
    };
    assert_eq!(
        undeclared,
        vec![format!("{GREET} (needs extensions.\"example:greeting\")")]
    );
}

#[test]
fn effective_grant____shared_buckets____granted_only_with_keyvalue_and_policy() {
    let spec = spec(json!({ "wasi:keyvalue": { "shared": { "points": "rw", "alarms": "ro" } } }));
    let mut with_kv = grants(&[Capability::KeyValue]);
    with_kv.buckets = BTreeSet::from(["points".to_string()]);
    let mut without_kv = grants(&[]);
    without_kv.buckets = BTreeSet::from(["points".to_string()]);

    let granted = effective_grant(&imports(&[KV]), &spec, &with_kv).unwrap();
    let no_kv = effective_grant(&imports(&[KV]), &spec, &without_kv).unwrap();

    assert_eq!(granted.granted_buckets, BTreeSet::from(["points".into()]));
    assert_eq!(granted.denied_buckets, BTreeSet::from(["alarms".into()]));
    assert!(no_kv.granted_buckets.is_empty());
    assert_eq!(no_kv.denied_buckets.len(), 2);
}

#[test]
fn effective_grant____http_incoming_without_imports____granted_or_denied_by_policy() {
    let spec =
        spec(json!({ "wasi:http/incoming-handler": { "routes": [{ "path_prefix": "/" }] } }));

    let granted = effective_grant(
        &imports(&[PROCESS]),
        &spec,
        &grants(&[Capability::HttpIncoming]),
    )
    .unwrap();
    let denied = effective_grant(&imports(&[PROCESS]), &spec, &grants(&[])).unwrap();

    assert_eq!(granted.granted, BTreeSet::from([Capability::HttpIncoming]));
    assert!(granted.unused.is_empty());
    assert_eq!(denied.denied, BTreeSet::from([Capability::HttpIncoming]));
}

#[test]
fn effective_grant____secrets____granted_only_with_capability_and_policy() {
    const SECRETS: &str = "paddock:secrets/store@0.1.0";
    let spec = spec(json!({ "paddock:secrets": { "secrets": ["db", "api"] } }));
    let mut operator = grants(&[Capability::Secrets]);
    operator.secrets = BTreeSet::from(["db".to_string()]);
    let mut no_capability = grants(&[]);
    no_capability.secrets = BTreeSet::from(["db".to_string()]);

    let granted = effective_grant(&imports(&[SECRETS]), &spec, &operator).unwrap();
    let without = effective_grant(&imports(&[SECRETS]), &spec, &no_capability).unwrap();

    assert_eq!(granted.granted_secrets, BTreeSet::from(["db".into()]));
    assert_eq!(granted.denied_secrets, BTreeSet::from(["api".into()]));
    assert!(without.granted_secrets.is_empty());
}
