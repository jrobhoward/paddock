#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use proptest::prelude::*;
use rstest::rstest;

use super::*;
use crate::protocol::{
    Answer, Bytes, Envelope, FromApp, KvAnswer, KvError, KvRequest, Pid, Request, ToApp, encode,
};

/// A frame's body: what follows its length prefix, checked against it.
fn body(frame: &[u8]) -> &[u8] {
    let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
    assert_eq!(frame.len(), 4 + len);
    &frame[4..]
}

fn request(op: KvOp, key: &str, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_kv_request(
        &mut out,
        &KvFrame {
            id: 7,
            bucket: 3,
            op,
            key,
            value,
        },
    )
    .unwrap();
    out
}

#[rstest]
#[case(KvOp::Get, "k000001", b"")]
#[case(KvOp::Set, "k", b"\x00\xffvalue")]
#[case(KvOp::Set, "", b"")]
#[case(KvOp::Delete, "ключ", b"")]
#[case(KvOp::Exists, "k", b"")]
fn write_kv_request____each_op____parses_back_unchanged(
    #[case] op: KvOp,
    #[case] key: &str,
    #[case] value: &[u8],
) {
    let frame = request(op, key, value);

    let found = parse_kv_request(body(&frame)).unwrap();

    assert_eq!(
        found,
        KvFrame {
            id: 7,
            bucket: 3,
            op,
            key,
            value,
        }
    );
    assert!(is_hot(body(&frame)));
}

#[test]
fn write_kv_request____get____layout_as_documented() {
    let frame = request(KvOp::Get, "ab", b"");

    assert_eq!(
        body(&frame),
        [0x01, 7, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 2, 0, b'a', b'b']
    );
}

#[test]
fn write_kv_request____key_over_u16____none_and_nothing_written() {
    let key = "k".repeat(70_000);
    let mut out = vec![9];

    let written = write_kv_request(
        &mut out,
        &KvFrame {
            id: 1,
            bucket: 0,
            op: KvOp::Get,
            key: &key,
            value: b"",
        },
    );

    assert!(written.is_none());
    assert_eq!(out, [9]);
}

#[rstest]
#[case(KvReply::Value(Some(b"\x00\x01v")))]
#[case(KvReply::Value(Some(b"")))]
#[case(KvReply::Value(None))]
#[case(KvReply::Unit)]
#[case(KvReply::Exists(true))]
#[case(KvReply::Exists(false))]
fn write_kv_answer____each_reply____parses_back_unchanged(#[case] reply: KvReply<'_>) {
    let mut frame = Vec::new();

    write_kv_answer(&mut frame, 42, &reply);

    assert_eq!(parse_kv_answer(body(&frame)).unwrap(), (42, reply));
}

#[test]
fn write_kv_answer____two_frames____appended() {
    let mut out = Vec::new();

    write_kv_answer(&mut out, 1, &KvReply::Unit);
    write_kv_answer(&mut out, 2, &KvReply::Value(None));

    let (first, second) = out.split_at(4 + 9);
    assert_eq!(parse_kv_answer(body(first)).unwrap(), (1, KvReply::Unit));
    assert_eq!(
        parse_kv_answer(body(second)).unwrap(),
        (2, KvReply::Value(None))
    );
}

#[rstest]
#[case::empty(&[])]
#[case::unknown_tag(&[0x05, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])]
#[case::truncated_id(&[0x01, 7, 0])]
#[case::key_past_end(&[0x01, 7, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 9, 0, b'a'])]
#[case::key_not_utf8(&[0x01, 7, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0xff])]
#[case::value_past_end(&[0x02, 7, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff])]
#[case::bytes_left_over(&[0x03, 7, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 1])]
fn parse_kv_request____malformed____error(#[case] bytes: &[u8]) {
    let found = parse_kv_request(bytes);

    assert!(matches!(found, Err(FrameError::Decode(_))), "{found:?}");
}

#[rstest]
#[case::empty(&[])]
#[case::unknown_tag(&[0x09, 1, 0, 0, 0, 0, 0, 0, 0])]
#[case::value_past_end(&[0x01, 1, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, b'a'])]
#[case::exists_not_a_bool(&[0x04, 1, 0, 0, 0, 0, 0, 0, 0, 2])]
#[case::bytes_left_over(&[0x03, 1, 0, 0, 0, 0, 0, 0, 0, 0])]
fn parse_kv_answer____malformed____error(#[case] bytes: &[u8]) {
    let found = parse_kv_answer(bytes);

    assert!(matches!(found, Err(FrameError::Decode(_))), "{found:?}");
}

#[rstest]
#[case(FromApp::Done)]
#[case(FromApp::Reply(None))]
#[case(FromApp::Request { id: 1, request: Request::SelfPid })]
#[case(FromApp::Request { id: 1, request: Request::KvOpen("b".into()) })]
#[case(FromApp::Request { id: 1, request: Request::Kv(KvRequest::Get { bucket: "b".into(), key: "k".into() }) })]
#[case(FromApp::Request { id: 1, request: Request::Link(Pid { node: 0, id: 0, incarnation: 0 }) })]
#[case(FromApp::Hello { version: 5 })]
fn is_hot____cbor_from_app____false(#[case] frame: FromApp) {
    let bytes = encode(&frame);

    assert!(!is_hot(body(&bytes)));
}

#[rstest]
#[case(ToApp::SaveSnapshot)]
#[case(ToApp::Ping { id: 0 })]
#[case(ToApp::Cast { msg: Envelope::new("t", vec![]) })]
#[case(ToApp::Answer { id: 1, answer: Answer::Unit })]
#[case(ToApp::Answer { id: 1, answer: Answer::KvOpened(Ok(0)) })]
#[case(ToApp::Answer { id: 1, answer: Answer::Kv(Err(KvError::AccessDenied)) })]
#[case(ToApp::Answer { id: 1, answer: Answer::Kv(Ok(KvAnswer::Value(Some(Bytes(vec![1]))))) })]
fn is_hot____cbor_to_app____false(#[case] frame: ToApp) {
    let bytes = encode(&frame);

    assert!(!is_hot(body(&bytes)));
}

proptest! {
    #[test]
    fn parse_kv_request____any_bytes____never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let _ = parse_kv_request(&bytes);
    }

    #[test]
    fn parse_kv_answer____any_bytes____never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let _ = parse_kv_answer(&bytes);
    }

    #[test]
    fn write_kv_request____any_key_and_value____parses_back(
        id in any::<u64>(),
        bucket in any::<u32>(),
        key in ".{0,40}",
        value in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut out = Vec::new();
        let frame = KvFrame { id, bucket, op: KvOp::Set, key: &key, value: &value };

        write_kv_request(&mut out, &frame).unwrap();

        prop_assert_eq!(parse_kv_request(body(&out)).unwrap(), frame);
    }
}

fn env(schema: Option<&str>, body: &[u8]) -> Envelope {
    Envelope {
        content_type: "application/cbor".into(),
        schema: schema.map(str::to_string),
        body: body.to_vec(),
    }
}

fn to() -> Pid {
    Pid {
        node: 3,
        id: u64::MAX - 1,
        incarnation: 2,
    }
}

#[rstest]
#[case(FromApp::Done)]
#[case(FromApp::Reply(None))]
#[case(FromApp::Reply(Some(env(None, b"\x00\x01"))))]
#[case(FromApp::Reply(Some(env(Some("app.v1.Point"), b""))))]
#[case(FromApp::Request { id: 9, request: Request::Cast { to: to(), msg: env(None, b"x") } })]
#[case(FromApp::Request { id: 9, request: Request::Call { to: to(), msg: env(Some(""), b"x"), timeout_ms: 5000 } })]
#[case(FromApp::Request { id: 9, request: Request::Reply { call_ref: 77, msg: env(None, b"") } })]
fn write_from_app____hot_frames____parse_back_unchanged(#[case] frame: FromApp) {
    let mut out = Vec::new();

    write_from_app(&mut out, &frame).unwrap();

    assert!(is_hot(body(&out)));
    assert_eq!(
        parse_from_app(body(&out)).unwrap(),
        FromAppHot::Frame(frame)
    );
}

#[rstest]
#[case(ToApp::Call { call_ref: 1, msg: env(None, b"\xff") })]
#[case(ToApp::Cast { msg: env(Some("s"), b"cast") })]
#[case(ToApp::Answer { id: 4, answer: Answer::Unit })]
#[case(ToApp::Answer { id: 4, answer: Answer::Called(Ok(env(None, b"reply"))) })]
fn write_to_app____hot_frames____parse_back_unchanged(#[case] frame: ToApp) {
    let mut out = Vec::new();

    write_to_app(&mut out, &frame).unwrap();

    assert!(is_hot(body(&out)));
    assert_eq!(parse_to_app(body(&out)).unwrap(), ToAppHot::Frame(frame));
}

#[rstest]
#[case(FromApp::Hello { version: 5 })]
#[case(FromApp::Failed("boom".into()))]
#[case(FromApp::Request { id: 1, request: Request::SelfPid })]
#[case(FromApp::Reply(Some(Envelope::new("t".repeat(70_000), vec![]))))]
fn write_from_app____not_covered____none_and_nothing_written(#[case] frame: FromApp) {
    let mut out = vec![1];

    let written = write_from_app(&mut out, &frame);

    assert!(written.is_none());
    assert_eq!(out, [1]);
}

#[rstest]
#[case(ToApp::SaveSnapshot)]
#[case(ToApp::Answer { id: 1, answer: Answer::Called(Err(crate::protocol::CallError::Timeout)) })]
#[case(ToApp::Answer { id: 1, answer: Answer::Ref(3) })]
#[case(ToApp::Cast { msg: Envelope { content_type: "t".into(), schema: Some("s".repeat(65_535)), body: vec![] } })]
fn write_to_app____not_covered____none_and_nothing_written(#[case] frame: ToApp) {
    let mut out = vec![1];

    let written = write_to_app(&mut out, &frame);

    assert!(written.is_none());
    assert_eq!(out, [1]);
}

#[test]
fn parse_to_app____kv_answer____borrowed_reply() {
    let mut out = Vec::new();
    write_kv_answer(&mut out, 5, &KvReply::Value(Some(b"v")));

    let found = parse_to_app(body(&out)).unwrap();

    assert_eq!(found, ToAppHot::Kv(5, KvReply::Value(Some(b"v"))));
}

#[test]
fn write_from_app____cast____layout_as_documented() {
    let frame = FromApp::Request {
        id: 1,
        request: Request::Cast {
            to: Pid {
                node: 2,
                id: 3,
                incarnation: 4,
            },
            msg: Envelope::new("t", b"b".to_vec()),
        },
    };
    let mut out = Vec::new();

    write_from_app(&mut out, &frame).unwrap();

    assert_eq!(
        body(&out),
        [
            0x08, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 1, 0,
            b't', 0xff, 0xff, 1, 0, 0, 0, b'b'
        ]
    );
}

#[rstest]
#[case::truncated_envelope(&[0x07, 1, 0])]
#[case::content_type_not_utf8(&[0x07, 1, 0, 0xff, 0xff, 0xff, 0, 0, 0, 0])]
#[case::body_past_end(&[0x07, 0, 0, 0xff, 0xff, 9, 0, 0, 0])]
#[case::cast_truncated_pid(&[0x08, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0])]
#[case::bytes_after_done(&[0x05, 0])]
#[case::unknown_tag(&[0x0b])]
fn parse_from_app____malformed____error(#[case] bytes: &[u8]) {
    let found = parse_from_app(bytes);

    assert!(matches!(found, Err(FrameError::Decode(_))), "{found:?}");
}

proptest! {
    #[test]
    fn parse_from_app____any_bytes____never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..96)) {
        let _ = parse_from_app(&bytes);
    }

    #[test]
    fn parse_to_app____any_bytes____never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..96)) {
        let _ = parse_to_app(&bytes);
    }
}
