#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use proptest::prelude::*;

fn entries(n: usize) -> Vec<(String, Vec<u8>)> {
    (0..n)
        .map(|i| (format!("k{i:06}"), format!("value {i}").into_bytes()))
        .collect()
}

#[test]
fn build____entries____each_read_back_and_the_rest_absent() {
    let all = entries(1000);
    let bytes = build(42, &all);

    let snap = Snapshot::parse(&bytes).unwrap();

    assert_eq!((snap.revision(), snap.len()), (42, 1000));
    for (k, v) in &all {
        assert_eq!(snap.get(k.as_bytes()), Some(v.as_slice()), "{k}");
    }
    assert_eq!(snap.get(b"k999999"), None);
    assert_eq!(snap.get(b""), None);
}

#[test]
fn build____empty____parses_and_holds_nothing() {
    let bytes = build(0, &[]);

    let snap = Snapshot::parse(&bytes).unwrap();

    assert!(snap.is_empty());
    assert_eq!(snap.get(b"any"), None);
}

#[test]
fn build____empty_key_and_value____read_back() {
    let bytes = build(1, &[(String::new(), Vec::new()), ("x".into(), vec![1])]);

    let snap = Snapshot::parse(&bytes).unwrap();

    assert_eq!(snap.get(b""), Some(&[][..]));
    assert_eq!(snap.get(b"x"), Some(&[1u8][..]));
}

#[test]
fn parse____not_a_snapshot____refused() {
    let mut bytes = build(1, &entries(3));
    bytes[0] = b'X';

    assert!(Snapshot::parse(&bytes).is_err());
    assert!(Snapshot::parse(&[]).is_err());
}

#[test]
fn parse____index_past_the_end____refused() {
    let mut bytes = build(1, &entries(3));
    let len = bytes.len();
    bytes.truncate(len - 8);

    assert!(Snapshot::parse(&bytes).is_err());
}

proptest! {
    #[test]
    fn get____any_bytes_after_a_valid_header____never_out_of_bounds(
        noise in proptest::collection::vec(any::<u8>(), 0..512),
        key in proptest::collection::vec(any::<u8>(), 0..16),
    ) {
        let mut bytes = build(7, &entries(4));
        let n = noise.len().min(bytes.len() - 64);
        bytes[64..64 + n].copy_from_slice(&noise[..n]);

        if let Ok(snap) = Snapshot::parse(&bytes) {
            let _ = snap.get(&key);
        }
    }

    #[test]
    fn build____distinct_keys____every_value_read_back(
        map in proptest::collection::btree_map("[a-z0-9]{0,12}", proptest::collection::vec(any::<u8>(), 0..32), 0..64),
    ) {
        let all: Vec<(String, Vec<u8>)> = map.into_iter().collect();
        let bytes = build(3, &all);
        let snap = Snapshot::parse(&bytes).unwrap();

        for (k, v) in &all {
            prop_assert_eq!(snap.get(k.as_bytes()), Some(v.as_slice()));
        }
    }
}
