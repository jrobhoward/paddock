#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use serde_json::json;

use super::*;

fn cbor<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).unwrap();
    out
}

#[test]
fn describe____sdk_snapshot____format_version_and_state() {
    #[derive(serde::Serialize)]
    struct State {
        count: u64,
        history: Vec<u64>,
    }
    let bytes = cbor(&(
        2u32,
        State {
            count: 3,
            history: vec![2, 3],
        },
    ));

    let d = describe("0.1.1", &bytes);

    assert_eq!(d.format, "paddock-sdk");
    assert_eq!(d.format_version, Some(2));
    assert_eq!(d.value, Some(json!({ "count": 3, "history": [2, 3] })));
}

#[test]
fn describe____decimal_text____text_not_cbor() {
    let d = describe("0.1.1", b"4");

    assert_eq!(d.format, "json");
    assert_eq!(d.value, Some(json!(4)));
}

#[test]
fn describe____plain_text____text() {
    let d = describe("0.1.1", b"hello world");

    assert_eq!(
        (d.format.as_str(), d.value),
        ("text", Some(json!("hello world")))
    );
}

#[test]
fn describe____binary____hex() {
    let d = describe("0.1.1", &[0xff, 0x00, 0xfe]);

    assert_eq!(
        (d.format.as_str(), d.hex.as_deref()),
        ("raw", Some("ff00fe"))
    );
}

#[test]
fn describe____empty____empty() {
    assert_eq!(describe("0.1.1", &[]).format, "empty");
}

#[test]
fn describe____little_endian_integer____hex_not_text_or_cbor() {
    let d = describe("0.1.1", &3u64.to_le_bytes());

    assert_eq!(
        (d.format.as_str(), d.hex.as_deref()),
        ("raw", Some("0300000000000000"))
    );
}

#[test]
fn describe____cbor_map____cbor() {
    let d = describe(
        "0.1.1",
        &cbor(&std::collections::BTreeMap::from([("a", 1)])),
    );

    assert_eq!(
        (d.format.as_str(), d.value),
        ("cbor", Some(json!({ "a": 1 })))
    );
}

#[test]
fn describe____map_key_is_map____hex_of_key_encoding() {
    // {{1: 2}: 3}
    let d = describe("0.1.1", &[0xa1, 0xa1, 0x01, 0x02, 0x03]);

    assert_eq!(
        (d.format.as_str(), d.value),
        ("cbor", Some(json!({ "a10102": 3 })))
    );
}

#[test]
fn describe____deeply_nested_map_keys____linear_key_size() {
    // Fuzz input (state_view timeout): maps nested ~40 deep, each the key
    // of the one above. Escaped JSON keys doubled per level.
    let bytes = [
        191, 1, 42, 51, 42, 45, 54, 161, 45, 54, 161, 1, 42, 161, 161, 161, 161, 161, 161, 161,
        161, 161, 161, 161, 161, 161, 1, 42, 161, 161, 161, 161, 161, 161, 161, 161, 161, 161, 161,
        161, 161, 161, 161, 161, 161, 161, 161, 161, 48, 45, 54, 57, 0, 128, 128, 128, 128, 128,
        128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 42,
        49, 42, 57, 49, 37, 57, 49, 37, 45, 51, 0, 255,
    ];

    let d = describe("0.1.1", &bytes);

    assert!(serde_json::to_string(&d).unwrap().len() < 100 * bytes.len());
}
