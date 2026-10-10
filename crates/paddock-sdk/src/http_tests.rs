#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn Request____envelope____round_trips() {
    let req = Request {
        method: "POST".into(),
        path: "/orders".into(),
        query: Some("a=1&b".into()),
        headers: vec![("X-Tenant".into(), "t1".into())],
        body: b"hi".to_vec(),
        key: Some("t1".into()),
    };

    let back = Request::from_envelope(&req.to_envelope()).unwrap();

    assert_eq!(back, req);
    assert_eq!(back.header("x-tenant"), Some("t1"));
    assert_eq!(back.param("a"), Some("1"));
    assert_eq!(back.param("b"), Some(""));
    assert_eq!(back.text(), Some("hi"));
}

#[test]
fn Request____other_envelope____none() {
    assert!(Request::from_envelope(&Envelope::text("GET /")).is_none());
}

#[test]
fn Response____builders____round_trip() {
    let r = Response::json(201, &serde_json::json!({ "id": 7 }))
        .unwrap()
        .header("x-a", "b");

    let back = Response::from_envelope(&r.clone().into()).unwrap();

    assert_eq!(back, r);
    assert_eq!(back.body, br#"{"id":7}"#);
}
