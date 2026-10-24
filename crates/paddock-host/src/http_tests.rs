#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

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

fn sample_headers() -> hyper::HeaderMap {
    let mut headers = hyper::HeaderMap::new();
    headers.insert("x-tenant", HeaderValue::from_static("t1"));
    headers.append("accept", HeaderValue::from_static("a"));
    headers.append("accept", HeaderValue::from_static("b"));
    headers.insert("x-raw", HeaderValue::from_bytes(b"caf\xe9").unwrap());
    headers
}

fn sample_request(headers: &hyper::HeaderMap) -> RequestEnvelope<'_> {
    RequestEnvelope {
        scheme: "https",
        method: "POST",
        path: "/orders",
        query: Some("a=1"),
        headers,
        body: b"hi",
        key: Some("t1".into()),
    }
}

#[test]
fn request_envelope____key_and_body____cbor_map_routable_by_key() {
    let headers = sample_headers();

    let env = request_envelope(&sample_request(&headers));
    let key = crate::pool::routing_key(&env, "key");

    assert_eq!(env.schema.as_deref(), Some(REQUEST_SCHEMA));
    assert_eq!(key.as_deref(), Some(b"t1".as_slice()));
}

/// The format apps read: the same bytes as a CBOR value tree built field
/// by field, as the envelope was encoded before requests were encoded
/// straight from the borrowed request.
#[rstest::rstest]
#[case::with_query_and_key(Some("a=1"), Some("t1"))]
#[case::without_either(None, None)]
fn request_envelope____encoded_directly____same_bytes_as_a_value_tree(
    #[case] query: Option<&str>,
    #[case] key: Option<&str>,
) {
    let headers = sample_headers();
    let request = RequestEnvelope {
        query,
        key: key.map(Into::into),
        ..sample_request(&headers)
    };
    let tree_headers = headers
        .iter()
        .map(|(k, v)| {
            Value::Array(vec![
                text(k.as_str()),
                text(&String::from_utf8_lossy(v.as_bytes())),
            ])
        })
        .collect();
    let mut map = vec![
        (text("scheme"), text("https")),
        (text("method"), text("POST")),
        (text("path"), text("/orders")),
        (text("query"), query.map_or(Value::Null, text)),
        (text("headers"), Value::Array(tree_headers)),
        (text("body"), Value::Bytes(b"hi".to_vec())),
    ];
    if let Some(key) = key {
        map.push((text("key"), text(key)));
    }
    let mut expected = Vec::new();
    ciborium::into_writer(&Value::Map(map), &mut expected).unwrap();

    let env = request_envelope(&request);

    assert_eq!(env.body, expected);
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
