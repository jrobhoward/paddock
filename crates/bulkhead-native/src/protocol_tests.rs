#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

fn pid() -> Pid {
    Pid {
        node: 1,
        id: 7,
        incarnation: 2,
    }
}

#[rstest]
#[case(ToApp::Init { args: b"{}".to_vec() })]
#[case(ToApp::Call { call_ref: 9, msg: Envelope::new("text/plain", b"hi".to_vec()) })]
#[case(ToApp::Info { info: Info::Down { reference: 3, pid: pid(), reason: ExitReason::Crashed("boom".into()) } })]
#[case(ToApp::Answer { id: 4, answer: Answer::Called(Err(CallError::Exited(ExitReason::Timeout))) })]
#[case(ToApp::Ping { id: 5 })]
#[case(ToApp::Info { info: Info::KvChanged { watch: 2, bucket: "b".into(), key: "k".into(), entry: Some((Bytes(vec![0, 255]), 7)), revision: 7 } })]
#[case(ToApp::Answer { id: 6, answer: Answer::Kv(Ok(KvAnswer::Values(vec![Some(Bytes(vec![1])), None]))) })]
#[case(ToApp::Answer { id: 7, answer: Answer::Kv(Ok(KvAnswer::Page(ScanPage { entries: vec![("k".into(), Bytes(vec![2]), 3)], next: Some("l".into()), revision: 3 }))) })]
#[case(ToApp::Answer { id: 8, answer: Answer::Kv(Err(KvError::Conflict(Some(4)))) })]
#[case(ToApp::Answer { id: 9, answer: Answer::Groups(Ok(GroupsAnswer::Members(vec![pid()]))) })]
#[case(ToApp::Answer { id: 10, answer: Answer::Secret(Err(SecretError::Unavailable("gone".into()))) })]
fn encode____to_app_frames____read_back_unchanged(#[case] frame: ToApp) {
    let bytes = encode(&frame);

    let found: ToApp = read(&mut bytes.as_slice()).unwrap().unwrap();

    assert_eq!(found, frame);
}

#[rstest]
#[case(FromApp::Hello { version: VERSION })]
#[case(FromApp::Reply(None))]
#[case(FromApp::Request { id: 1, request: Request::SendAfter { ms: 10, msg: Envelope::new("x", vec![0, 255]) } })]
#[case(FromApp::Request { id: 2, request: Request::Exit { to: pid(), reason: ExitReason::Killed } })]
#[case(FromApp::Log { level: Level::Warn, message: "careful".into() })]
#[case(FromApp::Request { id: 3, request: Request::Kv(KvRequest::SetMany { bucket: "b".into(), entries: vec![("k".into(), Bytes(vec![0, 255]))] }) })]
#[case(FromApp::Request { id: 5, request: Request::Groups(GroupsRequest::Publish { group: "g".into(), msg: Envelope::new("x", vec![1]) }) })]
#[case(FromApp::Request { id: 6, request: Request::MetricRegister { kind: MetricKind::Histogram, name: "lat".into(), help: "h".into(), labels: vec![("l".into(), "v".into())], buckets: vec![0.5, 1.0] } })]
#[case(FromApp::Metrics(vec![MetricUpdate::Add(1, 2.5), MetricUpdate::Observe(2, 0.25)]))]
#[case(FromApp::Request { id: 4, request: Request::Kv(KvRequest::Scan { bucket: "b".into(), query: ScanQuery { prefix: Some("p".into()), limit: 10, ..ScanQuery::default() } }) })]
fn encode____from_app_frames____read_back_unchanged(#[case] frame: FromApp) {
    let bytes = encode(&frame);

    let found: FromApp = read(&mut bytes.as_slice()).unwrap().unwrap();

    assert_eq!(found, frame);
}

#[test]
fn encode____envelope_body____cbor_byte_string_not_array() {
    let body = vec![200u8; 100];

    let bytes = encode(&Envelope::new("x", body));

    // As an array, each byte over 23 would take two: over 200 in all.
    assert!(bytes.len() < 4 + 100 + 50, "{} bytes", bytes.len());
}

#[test]
fn encode____bytes_in_a_list____cbor_byte_strings_not_arrays() {
    let value = KvAnswer::Values(vec![Some(Bytes(vec![1, 2, 3]))]);

    let cbor = encode(&value);

    // A byte string of three: 0x43 then the bytes.
    assert!(cbor.windows(4).any(|w| w == [0x43, 1, 2, 3]), "{cbor:x?}");
}

#[test]
fn read____empty_stream____none() {
    let found: Option<ToApp> = read(&mut [].as_slice()).unwrap();

    assert!(found.is_none());
}

#[test]
fn read____length_over_max____too_large() {
    let prefix = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes();

    let found = read::<ToApp>(&mut prefix.as_slice());

    assert!(matches!(found, Err(FrameError::TooLarge(_))));
}

#[test]
fn read____not_a_frame_of_the_type____decode_error() {
    let bytes = encode(&FromApp::Done);

    let found = read::<ToApp>(&mut bytes.as_slice());

    assert!(matches!(found, Err(FrameError::Decode(_))));
}
