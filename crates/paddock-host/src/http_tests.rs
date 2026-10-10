#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn route(host: Option<&str>, prefix: &str) -> HttpRoute {
    HttpRoute {
        host: host.map(str::to_string),
        path_prefix: prefix.to_string(),
        hash_header: None,
        max_body: 1024,
        timeout: DEFAULT_TIMEOUT,
    }
}

#[test]
fn prefix_matches____segment_boundaries____only_whole_segments() {
    assert!(prefix_matches("/orders", "/orders"));
    assert!(prefix_matches("/orders", "/orders/7"));
    assert!(!prefix_matches("/orders", "/ordersx"));
    assert!(prefix_matches("/", "/anything"));
    assert!(prefix_matches("/api/", "/api/x"));
    assert!(!prefix_matches("/orders", "/"));
}

#[test]
fn pick____several_routes____longest_prefix_then_named_host_wins() {
    let table = BTreeMap::from([
        ("root".to_string(), vec![route(None, "/")]),
        ("orders".to_string(), vec![route(None, "/orders")]),
        (
            "shop".to_string(),
            vec![route(Some("shop.example"), "/orders")],
        ),
    ]);

    let any = pick(&table, Some("other.example"), "/orders/1").unwrap().0;
    let named = pick(&table, Some("shop.example"), "/orders/1").unwrap().0;
    let fallback = pick(&table, None, "/health").unwrap().0;

    assert_eq!(any, "orders");
    assert_eq!(named, "shop");
    assert_eq!(fallback, "root");
}

#[test]
fn request_envelope____key_and_body____cbor_map_routable_by_key() {
    let env = request_envelope(
        "POST",
        "/orders",
        Some("a=1".into()),
        &[("x-tenant".into(), "t1".into())],
        b"hi",
        Some("t1".into()),
    );

    let key = crate::pool::routing_key(&env, "key");

    assert_eq!(env.schema.as_deref(), Some(REQUEST_SCHEMA));
    assert_eq!(key.as_deref(), Some(b"t1".as_slice()));
}

#[test]
fn parse_response____status_headers_body____becomes_the_response() {
    let value = Value::Map(vec![
        (text("status"), Value::Integer(201.into())),
        (
            text("headers"),
            Value::Array(vec![Value::Array(vec![text("x-a"), text("b")])]),
        ),
        (text("body"), Value::Bytes(b"made".to_vec())),
    ]);
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes).unwrap();

    let r = parse_response(&bytes).unwrap();

    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers()["x-a"], "b");
}

#[test]
fn parse_response____bad_status____none() {
    let value = Value::Map(vec![(text("status"), Value::Integer(42.into()))]);
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes).unwrap();

    assert!(parse_response(&bytes).is_none());
}
