//! Point values, what changed between scans, and the messages apps get.
//!
//! An update is one CBOR cast per device and scan, carrying only the points
//! that changed (every point when `full`), so a pool routed with
//! `hash:device` keeps each device on one instance and pays one message per
//! scan, not per point:
//!
//! ```text
//! { "device": "pump-1", "kind": "points", "full": false,
//!   "points": [ { "table": "holding", "address": 3, "value": 17 } ] }
//! { "device": "pump-1", "kind": "offline", "error": "connection refused" }
//! { "device": "server", "kind": "written",
//!   "points": [ { "table": "holding", "address": 0, "value": 5 } ] }
//! ```
//!
//! Coils and discrete inputs carry 0 or 1.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A Modbus table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Table {
    /// Holding registers (read-write words).
    Holding,
    /// Input registers (read-only words).
    Input,
    /// Coils (read-write bits).
    Coil,
    /// Discrete inputs (read-only bits).
    Discrete,
}

/// One point's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    /// Its table.
    pub table: Table,
    /// Its address.
    pub address: u16,
    /// Its value; 0 or 1 for coils and discrete inputs.
    pub value: u16,
}

/// What kind of message an [`Update`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A scan's points.
    Points,
    /// The device stopped answering; `error` says why. Sent once per
    /// outage; the next `points` after it is `full`.
    Offline,
    /// A master wrote to the node's server.
    Written,
}

/// A message to an app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    /// The device's name (the server's `name` for `written`): the routing
    /// key.
    pub device: String,
    /// What this is.
    pub kind: Kind,
    /// Whether `points` holds every polled point, not only changes.
    #[serde(default)]
    pub full: bool,
    /// The points.
    #[serde(default)]
    pub points: Vec<Point>,
    /// Why the device is offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Update {
    /// The CBOR body.
    #[must_use]
    pub fn to_cbor(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // Writing plain data to a Vec cannot fail.
        let _ = ciborium::into_writer(self, &mut out);
        out
    }
}

/// One scan's values, by table and address.
pub type Scan = BTreeMap<(Table, u16), u16>;

/// The points in `now` that differ from `before`, or all of them when
/// `full`.
#[must_use]
pub fn changes(before: &Scan, now: &Scan, full: bool) -> Vec<Point> {
    now.iter()
        .filter(|(at, value)| full || before.get(at) != Some(value))
        .map(|(&(table, address), &value)| Point {
            table,
            address,
            value,
        })
        .collect()
}

/// Record words read from `start` into `scan`.
pub(crate) fn record_words(scan: &mut Scan, table: Table, start: u16, words: &[u16]) {
    for (address, value) in (start..=u16::MAX).zip(words) {
        scan.insert((table, address), *value);
    }
}

/// Record bits read from `start` into `scan`, as 0 or 1.
pub(crate) fn record_bits(scan: &mut Scan, table: Table, start: u16, bits: &[bool]) {
    for (address, value) in (start..=u16::MAX).zip(bits) {
        scan.insert((table, address), u16::from(*value));
    }
}

#[cfg(test)]
#[path = "points_tests.rs"]
mod points_tests;
