#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use proptest::prelude::*;
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

fn arb_value() -> impl Strategy<Value = ciborium::Value> {
    use ciborium::Value;
    let leaf = prop_oneof![
        any::<i64>().prop_map(|i| Value::Integer(i.into())),
        any::<u64>().prop_map(|i| Value::Integer(i.into())),
        "[a-z]{0,6}".prop_map(Value::Text),
        proptest::collection::vec(any::<u8>(), 0..6).prop_map(Value::Bytes),
        any::<bool>().prop_map(Value::Bool),
        any::<f64>().prop_map(Value::Float),
        Just(Value::Null),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::vec((inner.clone(), inner.clone()), 0..4).prop_map(Value::Map),
            (0u64..40, inner).prop_map(|(t, v)| Value::Tag(t, Box::new(v))),
        ]
    })
}

proptest! {
    /// Maps with the field, without it, twice, under other keys, nested,
    /// tagged, and other values entirely, cut short or with bytes after:
    /// the walk agrees with decoding the whole body.
    #[test]
    fn routing_key____any_cbor____same_as_decoding_the_whole_body(
        entries in proptest::collection::vec(("(k|device|[a-z]{1,3})", arb_value()), 0..6),
        int_keys in proptest::collection::vec((any::<i64>(), arb_value()), 0..2),
        other in arb_value(),
        as_map in any::<bool>(),
        trailing in proptest::collection::vec(any::<u8>(), 0..3),
        cut in 0usize..3,
    ) {
        use ciborium::Value;
        let value = if as_map {
            let mut map: Vec<(Value, Value)> =
                entries.into_iter().map(|(k, v)| (Value::Text(k), v)).collect();
            map.extend(int_keys.into_iter().map(|(k, v)| (Value::Integer(k.into()), v)));
            Value::Map(map)
        } else {
            other
        };
        let mut body = cbor(&value);
        if cut == 1 {
            body.pop();
        }
        body.extend(trailing);
        let msg = Envelope::new("application/cbor", body.clone());

        prop_assert_eq!(routing_key(&msg, "k"), tree_key(&body, "k"));
        prop_assert_eq!(routing_key(&msg, "device"), tree_key(&body, "device"));
    }

    /// Any bytes at all.
    #[test]
    fn routing_key____random_bytes____same_as_decoding_the_whole_body(
        body in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let msg = Envelope::new("application/cbor", body.clone());

        prop_assert_eq!(routing_key(&msg, "k"), tree_key(&body, "k"));
    }
}

/// Encodings ciborium's writer never makes: lengths not known up front,
/// segmented strings, the field after a nested indefinite array.
#[rstest]
// {_ "k": "ab"} (indefinite map)
#[case(&[0xbf, 0x61, b'k', 0x62, b'a', b'b', 0xff], Some(b"ab".to_vec()))]
// {"k": (_ "a" "b")} (segmented text)
#[case(&[0xa1, 0x61, b'k', 0x7f, 0x61, b'a', 0x61, b'b', 0xff], Some(b"ab".to_vec()))]
// {"x": [_ 1, 2], "k": h'0102'} (indefinite array, then bytes)
#[case(&[0xa2, 0x61, b'x', 0x9f, 0x01, 0x02, 0xff, 0x61, b'k', 0x42, 0x01, 0x02], Some(vec![1, 2]))]
// {"k": -5}
#[case(&[0xa1, 0x61, b'k', 0x24], Some(b"-5".to_vec()))]
// {_ "k": 1 (no break: cut short)
#[case(&[0xbf, 0x61, b'k', 0x01], None)]
// {"k": 1, "z": undefined}
#[case(&[0xa2, 0x61, b'k', 0x01, 0x61, b'z', 0xf7], None)]
fn routing_key____unusual_encodings____same_as_decoding_the_whole_body(
    #[case] body: &[u8],
    #[case] expected: Option<Vec<u8>>,
) {
    let msg = Envelope::new("application/cbor", body.to_vec());

    let key = routing_key(&msg, "k");

    assert_eq!(key, tree_key(body, "k"));
    if body.last() != Some(&0xf7) {
        assert_eq!(key, expected);
    }
}

#[test]
fn walk____plain_map____answers_itself_without_the_value_tree() {
    let value = ciborium::Value::Map(vec![
        (
            ciborium::Value::Text("points".into()),
            ciborium::Value::Array(vec![ciborium::Value::Integer(1.into()); 125]),
        ),
        (
            ciborium::Value::Text("device".into()),
            ciborium::Value::Text("pump-1".into()),
        ),
    ]);
    let body = cbor(&value);

    let walked = walk::field(&body, "device");

    assert_eq!(walked, Ok(Some(b"pump-1".to_vec())));
}

#[test]
fn to_new____no_key____the_share_asked_of_every_hundred() {
    let next = AtomicUsize::new(0);

    let at = |percent| (0..1000).filter(|_| to_new(None, percent, &next)).count();

    assert_eq!([at(0), at(10), at(50), at(100)], [0, 100, 500, 1000]);
}

#[test]
fn to_new____no_key____turns_spread_not_in_a_burst() {
    let next = AtomicUsize::new(0);

    let half: Vec<bool> = (0..6).map(|_| to_new(None, 50, &next)).collect();
    let tenth = (0..10).filter(|_| to_new(None, 10, &next)).count();

    assert_eq!(half, [false, true, false, true, false, true]);
    assert_eq!(tenth, 1);
}

#[test]
fn to_new____key____same_side_every_time_and_moves_once_as_the_weight_rises() {
    let next = AtomicUsize::new(0);
    let keys: Vec<String> = (0..2000).map(|i| format!("key-{i}")).collect();

    let at = |percent| -> Vec<bool> {
        keys.iter()
            .map(|k| to_new(Some(k.as_bytes()), percent, &next))
            .collect()
    };
    let (ten, ten_again, fifty) = (at(10), at(10), at(50));

    assert_eq!(ten, ten_again);
    assert!(
        ten.iter().zip(&fifty).all(|(t, f)| !t || *f),
        "a key on the new side stays"
    );
    let share = |v: &[bool]| v.iter().filter(|b| **b).count();
    assert!((150..250).contains(&share(&ten)), "{}", share(&ten));
    assert!((900..1100).contains(&share(&fifty)), "{}", share(&fifty));
}

#[test]
fn to_new____keys_on_the_new_side____spread_over_its_members() {
    let next = AtomicUsize::new(0);

    let mut per_member = [0usize; 4];
    for i in 0..4000 {
        let key = format!("key-{i}");
        if to_new(Some(key.as_bytes()), 10, &next) {
            per_member[pick_index(4, Some(key.as_bytes()), &next).unwrap()] += 1;
        }
    }

    assert!(
        per_member.iter().all(|&n| (60..140).contains(&n)),
        "{per_member:?}"
    );
}
