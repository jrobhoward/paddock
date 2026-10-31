#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use serde::{Deserialize, Serialize};

use super::*;
use crate::Snapshot as DeriveSnapshot;

#[derive(Debug, PartialEq, Serialize, Deserialize, DeriveSnapshot)]
struct V1 {
    count: u64,
}

#[derive(Debug, PartialEq, Serialize, Deserialize, DeriveSnapshot)]
#[snapshot(version = 2, migrate = from_v1)]
struct V2 {
    count: u64,
    history: Vec<u64>,
}

fn from_v1(from: u32, old: Value) -> Result<V2, String> {
    match from {
        1 => {
            let v1: V1 = old.deserialized().map_err(|e| e.to_string())?;
            Ok(V2 {
                count: v1.count,
                history: Vec::new(),
            })
        }
        other => Err(format!("unknown format {other}")),
    }
}

#[test]
fn Snapshot____same_version____round_trips() {
    let state = V2 {
        count: 3,
        history: vec![2, 3],
    };

    let back = V2::from_snapshot(&state.to_snapshot().unwrap()).unwrap();

    assert_eq!(back, state);
}

#[test]
fn Snapshot____older_version____migrated() {
    let bytes = V1 { count: 41 }.to_snapshot().unwrap();

    let back = V2::from_snapshot(&bytes).unwrap();

    assert_eq!(
        back,
        V2 {
            count: 41,
            history: vec![]
        }
    );
}

#[test]
fn Snapshot____newer_version_without_migrate____refused() {
    let bytes = V2 {
        count: 1,
        history: vec![],
    }
    .to_snapshot()
    .unwrap();

    let err = V1::from_snapshot(&bytes).unwrap_err();

    assert_eq!(
        err,
        "cannot read snapshot format 2 (this version writes format 1)"
    );
}

#[test]
fn Snapshot____not_cbor____refused() {
    let err = V1::from_snapshot(b"\xff\xff").unwrap_err();

    assert!(err.starts_with("not a snapshot"), "{err}");
}

#[test]
fn Snapshot____derive_default_version____is_one() {
    assert_eq!(V1::VERSION, 1);
    assert_eq!(V2::VERSION, 2);
}
