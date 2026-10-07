#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

fn cbor(value: &ciborium::Value) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).unwrap();
    out
}

fn cbor_map(key: &str, value: ciborium::Value) -> Envelope {
    let map = ciborium::Value::Map(vec![(ciborium::Value::Text(key.into()), value)]);
    Envelope::new("application/cbor", cbor(&map))
}

#[test]
fn routing_key____json_string_field____its_utf8() {
    let msg = Envelope::new(
        "application/json",
        br#"{"customer_id":"c-42","x":1}"#.to_vec(),
    );

    let key = routing_key(&msg, "customer_id");

    assert_eq!(key, Some(b"c-42".to_vec()));
}

#[test]
fn routing_key____json_with_charset_parameter____still_read() {
    let msg = Envelope::new("application/json; charset=utf-8", br#"{"k":7}"#.to_vec());

    let key = routing_key(&msg, "k");

    assert_eq!(key, Some(b"7".to_vec()));
}

#[rstest]
#[case(ciborium::Value::Text("c-42".into()), b"c-42".to_vec())]
#[case(ciborium::Value::Integer(7.into()), b"7".to_vec())]
#[case(ciborium::Value::Bytes(vec![1, 2]), vec![1, 2])]
fn routing_key____cbor_field____bytes_match_json_rendering(
    #[case] value: ciborium::Value,
    #[case] expected: Vec<u8>,
) {
    let msg = cbor_map("k", value);

    let key = routing_key(&msg, "k");

    assert_eq!(key, Some(expected));
}

#[rstest]
#[case(Envelope::new("text/plain", b"customer_id=1".to_vec()))]
#[case(Envelope::new("application/json", b"not json".to_vec()))]
#[case(Envelope::new("application/json", br#"{"other":1}"#.to_vec()))]
#[case(Envelope::new("application/json", br#"[1,2]"#.to_vec()))]
#[case(Envelope::new("application/json", br#"{"customer_id":{"nested":1}}"#.to_vec()))]
#[case(Envelope::new("application/cbor", vec![0xff]))]
fn routing_key____no_usable_field____none(#[case] msg: Envelope) {
    assert_eq!(routing_key(&msg, "customer_id"), None);
}

#[test]
fn pick_index____no_key____cycles_through_members() {
    let next = AtomicUsize::new(0);

    let picks: Vec<usize> = (0..6)
        .map(|_| pick_index(3, None, &next).unwrap())
        .collect();

    assert_eq!(picks, vec![0, 1, 2, 0, 1, 2]);
}

#[test]
fn pick_index____same_key____same_member_every_time() {
    let next = AtomicUsize::new(0);

    let picks: Vec<usize> = (0..5)
        .map(|_| pick_index(4, Some(b"c-42"), &next).unwrap())
        .collect();

    assert!(picks.windows(2).all(|w| w[0] == w[1]), "{picks:?}");
}

#[test]
fn pick_index____many_keys____spread_over_members() {
    let next = AtomicUsize::new(0);

    let used: std::collections::BTreeSet<usize> = (0..100)
        .map(|i| pick_index(4, Some(format!("key-{i}").as_bytes()), &next).unwrap())
        .collect();

    assert_eq!(used.len(), 4);
}

#[test]
fn pick_index____empty_pool____none() {
    let next = AtomicUsize::new(0);

    assert_eq!(pick_index(0, None, &next), None);
}
