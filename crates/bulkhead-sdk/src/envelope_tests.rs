#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use serde::{Deserialize, Serialize};

use super::*;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Order {
    id: u64,
    item: String,
}

#[test]
fn Envelope____cbor_round_trip____same_value() {
    let order = Order {
        id: 7,
        item: "widget".into(),
    };

    let env = Envelope::cbor(&order).unwrap();
    let back: Order = env.decode().unwrap();

    assert_eq!(env.content_type, CBOR);
    assert_eq!(back, order);
}

#[test]
fn Envelope____json_with_charset____decodes() {
    let mut env = Envelope::json(&Order {
        id: 1,
        item: "x".into(),
    })
    .unwrap();
    env.content_type = "application/json; charset=utf-8".into();

    let back: Order = env.decode().unwrap();

    assert_eq!(back.id, 1);
}

#[test]
fn Envelope____text____as_text_only_for_text() {
    let text = Envelope::text("hello");
    let cbor = Envelope::cbor(&"hello").unwrap();

    assert_eq!(text.as_text(), Some("hello"));
    assert_eq!(cbor.as_text(), None);
}

#[test]
fn Envelope____decode_unknown_type____error_names_it() {
    let env = Envelope::new("application/x-custom", vec![1, 2]);

    let err = env.decode::<u8>().unwrap_err();

    assert_eq!(
        err.to_string(),
        "cannot decode content type 'application/x-custom'"
    );
}

#[test]
fn Envelope____with_schema____kept() {
    let env = Envelope::text("x").with_schema("orders.v1");

    assert_eq!(env.schema.as_deref(), Some("orders.v1"));
}
