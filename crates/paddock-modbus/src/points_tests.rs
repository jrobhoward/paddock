#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn scan(points: &[(Table, u16, u16)]) -> Scan {
    points.iter().map(|&(t, a, v)| ((t, a), v)).collect()
}

#[test]
fn changes____one_value_differs____only_that_point() {
    let before = scan(&[(Table::Holding, 0, 1), (Table::Holding, 1, 2)]);
    let now = scan(&[(Table::Holding, 0, 1), (Table::Holding, 1, 3)]);

    let changed = changes(&before, &now, false);

    assert_eq!(
        changed,
        vec![Point {
            table: Table::Holding,
            address: 1,
            value: 3
        }]
    );
}

#[test]
fn changes____full____every_point() {
    let now = scan(&[(Table::Coil, 0, 1), (Table::Input, 5, 9)]);

    let changed = changes(&now.clone(), &now, true);

    assert_eq!(changed.len(), 2);
}

#[test]
fn changes____first_scan____every_point_is_new() {
    let now = scan(&[(Table::Discrete, 7, 0)]);

    let changed = changes(&Scan::new(), &now, false);

    assert_eq!(changed.len(), 1);
}

#[test]
fn record____words_and_bits____addressed_from_start() {
    let mut s = Scan::new();

    record_words(&mut s, Table::Holding, 10, &[100, 200]);
    record_bits(&mut s, Table::Coil, 0, &[true, false]);

    assert_eq!(
        s,
        scan(&[
            (Table::Holding, 10, 100),
            (Table::Holding, 11, 200),
            (Table::Coil, 0, 1),
            (Table::Coil, 1, 0),
        ])
    );
}

#[test]
fn Update____cbor____round_trips_with_device_at_top_level() {
    let update = Update {
        device: "pump-1".into(),
        kind: Kind::Points,
        full: true,
        points: vec![Point {
            table: Table::Holding,
            address: 3,
            value: 17,
        }],
        error: None,
    };

    let bytes = update.to_cbor();
    let back: Update = ciborium::from_reader(bytes.as_slice()).unwrap();
    let map: ciborium::Value = ciborium::from_reader(bytes.as_slice()).unwrap();

    assert_eq!(back, update);
    let device = map
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_text() == Some("device"))
        .map(|(_, v)| v.as_text().unwrap().to_string());
    assert_eq!(device.as_deref(), Some("pump-1"));
}
