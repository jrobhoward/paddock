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

#[test]
fn effective_grant____declared_and_granted____links_host_implementation() {
    let spec = spec(json!({ "wasi:keyvalue": {} }));

    let grant = effective_grant(
        &imports(&[PROCESS, KV]),
        &spec,
        &BTreeSet::from([Capability::KeyValue]),
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

    let grant = effective_grant(&imports(&[KV]), &spec, &BTreeSet::new()).unwrap();

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
        &BTreeSet::from([Capability::Logging, Capability::KeyValue]),
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
        &BTreeSet::new(),
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
        &BTreeSet::from([Capability::HttpOutgoing]),
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

    let err = effective_grant(&imports(&[HTTP_TYPES]), &spec, &BTreeSet::new()).unwrap_err();

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
    let grant = effective_grant(&imports(&[PROCESS, KV]), &spec, &BTreeSet::new()).unwrap();

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
