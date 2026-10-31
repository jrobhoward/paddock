#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

//! The SDK against the examples in `docs/GUEST_FORMATS.md`.

use serde::{Deserialize, Serialize};

use crate::Snapshot as DeriveSnapshot;
use crate::batch;
use crate::envelope::Envelope;

const FORMATS: &str = include_str!("../../../docs/GUEST_FORMATS.md");

/// The body of the fenced block ` ```<kind> <name> ` in GUEST_FORMATS.md:
/// bytes from hex for `cbor-hex`, the text as is otherwise.
fn example(kind: &str, name: &str) -> Vec<u8> {
    let fence = format!("```{kind} {name}\n");
    let start = FORMATS
        .find(&fence)
        .unwrap_or_else(|| panic!("no example {kind} {name}"))
        + fence.len();
    let text = &FORMATS[start..start + FORMATS[start..].find("```").unwrap()];
    if kind != "cbor-hex" {
        return text.trim().as_bytes().to_vec();
    }
    let hex: String = text.split_whitespace().collect();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[derive(Debug, PartialEq, Serialize, Deserialize, DeriveSnapshot)]
#[snapshot(version = 2, migrate = refuse)]
struct Board {
    count: u64,
    label: String,
}

fn refuse(from: u32, _old: crate::snapshot::Value) -> Result<Board, String> {
    Err(format!("format {from}"))
}

#[test]
fn snapshot____documented_example____written_and_read_alike() {
    let board = Board {
        count: 5,
        label: "board".into(),
    };
    let documented = example("cbor-hex", "snapshot");

    let written = board.to_snapshot().unwrap();
    let read = Board::from_snapshot(&documented).unwrap();

    assert_eq!(written, documented);
    assert_eq!(read, board);
}

#[cfg(feature = "http")]
#[test]
fn http_request____documented_example____read_with_every_field() {
    use crate::http::{REQUEST_SCHEMA, Request};
    let msg = Envelope::new("application/cbor", example("cbor-hex", "http-request"))
        .with_schema(REQUEST_SCHEMA);

    let request = Request::from_envelope(&msg).unwrap();

    assert_eq!(request.scheme, "http");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/board/inc");
    assert_eq!(request.query.as_deref(), Some("by=2"));
    assert_eq!(
        request.headers,
        vec![
            ("host".to_string(), "localhost:8080".to_string()),
            ("content-type".to_string(), "text/plain".to_string()),
        ]
    );
    assert_eq!(request.body, b"hi");
    assert_eq!(request.key.as_deref(), Some("tenant-7"));
}

#[cfg(feature = "http")]
#[test]
fn http_response____documented_example____same_bytes() {
    use crate::http::{RESPONSE_SCHEMA, Response};

    let msg = Response::new(201)
        .header("content-type", "text/plain")
        .body("ok")
        .to_envelope();

    assert_eq!(msg.content_type, "application/cbor");
    assert_eq!(msg.schema.as_deref(), Some(RESPONSE_SCHEMA));
    assert_eq!(msg.body, example("cbor-hex", "http-response"));
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Item {
    device: String,
    value: u16,
}

#[test]
fn batch____documented_example____same_bytes_and_read_back() {
    let items = vec![
        Item {
            device: "pump-1".into(),
            value: 17,
        },
        Item {
            device: "pump-2".into(),
            value: 3,
        },
    ];
    let documented = example("cbor-hex", "batch");

    let msg = batch::envelope(&items).unwrap();
    let read: Vec<Item> = batch::items(
        &Envelope::new("application/cbor", documented.clone()).with_schema(batch::SCHEMA),
    )
    .unwrap()
    .unwrap();

    assert_eq!(msg.body, documented);
    assert_eq!(msg.schema.as_deref(), Some(batch::SCHEMA));
    assert_eq!(read, items);
}

#[cfg(feature = "watch")]
#[test]
fn kv_changed____documented_example____read_as_a_change() {
    use crate::types::Info;
    use crate::watch::{self, Change};
    let info = Info::Event {
        name: watch::EVENT.into(),
        msg: Envelope::new("application/json", example("json", "kv-changed")),
    };

    let change = Change::from_info(&info).unwrap();

    assert_eq!(
        change,
        Change {
            watch: 1,
            bucket: "board".into(),
            key: "count".into(),
            revision: 7,
        }
    );
}

#[cfg(feature = "net")]
#[test]
fn io_ready____documented_example____its_tag() {
    use crate::net::{READY_EVENT, ready_tag};
    use crate::types::Info;
    let info = Info::Event {
        name: READY_EVENT.into(),
        msg: Envelope::new("application/json", example("json", "io-ready")),
    };

    let tag = ready_tag(&info);

    assert_eq!(tag, Some(3));
}
