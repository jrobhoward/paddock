//! The protocol as the contract (ROADMAP Z9a): every frame of the
//! versions a node accepts, encoded, held as golden bytes in
//! `tests/protocol/v<N>.txt`.
//!
//! - The current version's samples must encode to exactly its file's
//!   bytes, so a change to an existing frame fails here; a new frame needs
//!   a sample (the name functions below are exhaustive, and every name
//!   must have one) and a new line (`BULKHEAD_BLESS_PROTOCOL=1` rewrites
//!   the current version's file; review the diff).
//! - The previous version's frames, which apps built against it still
//!   send and expect, must decode with the current types and encode back
//!   to the same bytes. A new version starts its file as a copy of the
//!   last one's lines plus its new frames, and the file of the version
//!   before the window is deleted.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use bulkhead_native::hot::{self, KvFrame, KvOp, KvReply};
use bulkhead_native::protocol::{
    self, Answer, Bytes, CallError, CastError, ConfigError, Envelope, ExitReason, ExtensionError,
    FdRef, FromApp, GroupsAnswer, GroupsRequest, HttpRequest, HttpResponse, Info, KvAnswer,
    KvError, KvRequest, Level, MIN_VERSION, MetricError, MetricKind, MetricUpdate, NetAnswer,
    NetRequest, NetStream, Pid, RegisterError, Request, SecretError, SqlAnswer, SqlRequest,
    SqlRows, SqlValue, ToApp, VERSION,
};

fn file(version: u16) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/protocol/v{version}.txt"))
}

fn pid() -> Pid {
    Pid {
        node: 1,
        id: 7,
        incarnation: 2,
    }
}

fn env() -> Envelope {
    Envelope::new("text/plain", b"hi".to_vec())
}

fn stream() -> NetStream {
    NetStream {
        socket: 2,
        fd: FdRef(5),
        local: "127.0.0.1:4000".parse().unwrap(),
        peer: "[::1]:80".parse().unwrap(),
    }
}

fn to_app_name(f: &ToApp) -> &'static str {
    match f {
        ToApp::Init { .. } => "Init",
        ToApp::LoadSnapshot { .. } => "LoadSnapshot",
        ToApp::SaveSnapshot => "SaveSnapshot",
        ToApp::Call { .. } => "Call",
        ToApp::Cast { .. } => "Cast",
        ToApp::Info { .. } => "Info",
        ToApp::Connection { .. } => "Connection",
        ToApp::Terminate { .. } => "Terminate",
        ToApp::Answer { .. } => "Answer",
        ToApp::Ping { .. } => "Ping",
        ToApp::Published { .. } => "Published",
    }
}

fn info_name(i: &Info) -> &'static str {
    match i {
        Info::Exit { .. } => "Exit",
        Info::Down { .. } => "Down",
        Info::Timer { .. } => "Timer",
        Info::KvChanged { .. } => "KvChanged",
        Info::HostEvent { .. } => "HostEvent",
    }
}

fn from_app_name(f: &FromApp) -> &'static str {
    match f {
        FromApp::Hello { .. } => "Hello",
        FromApp::Init(_) => "Init",
        FromApp::Snapshot(_) => "Snapshot",
        FromApp::Reply(_) => "Reply",
        FromApp::Done => "Done",
        FromApp::Failed(_) => "Failed",
        FromApp::Request { .. } => "Request",
        FromApp::Log { .. } => "Log",
        FromApp::Pong { .. } => "Pong",
        FromApp::Metrics(_) => "Metrics",
    }
}

fn request_name(r: &Request) -> &'static str {
    match r {
        Request::SelfPid => "SelfPid",
        Request::Cast { .. } => "Cast",
        Request::CastWait { .. } => "CastWait",
        Request::Call { .. } => "Call",
        Request::Reply { .. } => "Reply",
        Request::Link(_) => "Link",
        Request::Unlink(_) => "Unlink",
        Request::TrapExits(_) => "TrapExits",
        Request::Monitor(_) => "Monitor",
        Request::Demonitor(_) => "Demonitor",
        Request::Register(_) => "Register",
        Request::Whereis(_) => "Whereis",
        Request::SendAfter { .. } => "SendAfter",
        Request::CancelTimer(_) => "CancelTimer",
        Request::Exit { .. } => "Exit",
        Request::Kv(_) => "Kv",
        Request::KvOpen(_) => "KvOpen",
        Request::Groups(_) => "Groups",
        Request::ConfigGet(_) => "ConfigGet",
        Request::ConfigAll => "ConfigAll",
        Request::Secret(_) => "Secret",
        Request::MetricRegister { .. } => "MetricRegister",
        Request::Sql(_) => "Sql",
        Request::Net(_) => "Net",
        Request::Http(_) => "Http",
        Request::Extension { .. } => "Extension",
    }
}

fn answer_name(a: &Answer) -> &'static str {
    match a {
        Answer::Unit => "Unit",
        Answer::Pid(_) => "Pid",
        Answer::MaybePid(_) => "MaybePid",
        Answer::Ref(_) => "Ref",
        Answer::Called(_) => "Called",
        Answer::CastFailed(_) => "CastFailed",
        Answer::Registered(_) => "Registered",
        Answer::Kv(_) => "Kv",
        Answer::KvOpened(_) => "KvOpened",
        Answer::Groups(_) => "Groups",
        Answer::Config(_) => "Config",
        Answer::ConfigAll(_) => "ConfigAll",
        Answer::Secret(_) => "Secret",
        Answer::Metric(_) => "Metric",
        Answer::Sql(_) => "Sql",
        Answer::Net(_) => "Net",
        Answer::Http(_) => "Http",
        Answer::Extension(_) => "Extension",
    }
}

/// Every name the name functions give, by frame type.
const NAMES: &[(&str, &[&str])] = &[
    (
        "ToApp",
        &[
            "Init",
            "LoadSnapshot",
            "SaveSnapshot",
            "Call",
            "Cast",
            "Info",
            "Connection",
            "Terminate",
            "Answer",
            "Ping",
            "Published",
        ],
    ),
    ("Info", &["Exit", "Down", "Timer", "KvChanged", "HostEvent"]),
    (
        "FromApp",
        &[
            "Hello", "Init", "Snapshot", "Reply", "Done", "Failed", "Request", "Log", "Pong",
            "Metrics",
        ],
    ),
    (
        "Request",
        &[
            "SelfPid",
            "Cast",
            "CastWait",
            "Call",
            "Reply",
            "Link",
            "Unlink",
            "TrapExits",
            "Monitor",
            "Demonitor",
            "Register",
            "Whereis",
            "SendAfter",
            "CancelTimer",
            "Exit",
            "Kv",
            "KvOpen",
            "Groups",
            "ConfigGet",
            "ConfigAll",
            "Secret",
            "MetricRegister",
            "Sql",
            "Net",
            "Http",
            "Extension",
        ],
    ),
    (
        "Answer",
        &[
            "Unit",
            "Pid",
            "MaybePid",
            "Ref",
            "Called",
            "CastFailed",
            "Registered",
            "Kv",
            "KvOpened",
            "Groups",
            "Config",
            "ConfigAll",
            "Secret",
            "Metric",
            "Sql",
            "Net",
            "Http",
            "Extension",
        ],
    ),
];

fn to_apps() -> Vec<ToApp> {
    vec![
        ToApp::Init {
            args: b"{}".to_vec(),
        },
        ToApp::LoadSnapshot {
            from_version: "1.0.0".into(),
            snapshot: vec![0, 255],
        },
        ToApp::SaveSnapshot,
        ToApp::Call {
            call_ref: 9,
            msg: env(),
        },
        ToApp::Cast { msg: env() },
        ToApp::Info {
            info: Info::Down {
                reference: 3,
                pid: pid(),
                reason: ExitReason::Crashed("boom".into()),
            },
        },
        ToApp::Connection {
            listener: "lines".into(),
            stream: stream(),
        },
        ToApp::Terminate {
            reason: ExitReason::Shutdown,
        },
        ToApp::Answer {
            id: 4,
            answer: Answer::Unit,
        },
        ToApp::Ping { id: 5 },
        ToApp::Published {
            bucket: "b".into(),
            revision: 3,
            fd: FdRef(1),
        },
    ]
}

fn infos() -> Vec<Info> {
    vec![
        Info::Exit {
            from: pid(),
            reason: ExitReason::Killed,
        },
        Info::Down {
            reference: 1,
            pid: pid(),
            reason: ExitReason::Noproc,
        },
        Info::Timer {
            reference: 2,
            msg: env(),
        },
        Info::KvChanged {
            watch: 2,
            bucket: "b".into(),
            key: "k".into(),
            entry: Some((Bytes(vec![0, 255]), 7)),
            revision: 7,
        },
        Info::HostEvent {
            name: "io-ready".into(),
            msg: Envelope::new("application/json", br#"{"tag":3}"#.to_vec()),
        },
    ]
}

fn from_apps() -> Vec<FromApp> {
    vec![
        FromApp::Hello { version: VERSION },
        FromApp::Init(Err("bad args".into())),
        FromApp::Snapshot(Bytes(vec![0, 255])),
        FromApp::Reply(Some(env())),
        FromApp::Done,
        FromApp::Failed("panic: x".into()),
        FromApp::Request {
            id: 1,
            request: Request::SelfPid,
        },
        FromApp::Log {
            level: Level::Warn,
            message: "careful".into(),
        },
        FromApp::Pong { id: 5 },
        FromApp::Metrics(vec![MetricUpdate::Add(1, 2.5)]),
    ]
}

fn requests() -> Vec<Request> {
    vec![
        Request::SelfPid,
        Request::Cast {
            to: pid(),
            msg: env(),
        },
        Request::CastWait {
            to: pid(),
            msg: env(),
            timeout_ms: 100,
        },
        Request::Call {
            to: pid(),
            msg: env(),
            timeout_ms: 5000,
        },
        Request::Reply {
            call_ref: 3,
            msg: env(),
        },
        Request::Link(pid()),
        Request::Unlink(pid()),
        Request::TrapExits(true),
        Request::Monitor(pid()),
        Request::Demonitor(4),
        Request::Register("name".into()),
        Request::Whereis("name".into()),
        Request::SendAfter { ms: 10, msg: env() },
        Request::CancelTimer(6),
        Request::Exit {
            to: pid(),
            reason: ExitReason::Normal,
        },
        Request::Kv(KvRequest::Get {
            bucket: "b".into(),
            key: "k".into(),
        }),
        Request::KvOpen("b".into()),
        Request::Groups(GroupsRequest::Publish {
            group: "g".into(),
            msg: env(),
        }),
        Request::ConfigGet("mode".into()),
        Request::ConfigAll,
        Request::Secret("token".into()),
        Request::MetricRegister {
            kind: MetricKind::Histogram,
            name: "lat".into(),
            help: "h".into(),
            labels: vec![("l".into(), "v".into())],
            buckets: vec![0.5, 1.0],
        },
        Request::Sql(SqlRequest::Query {
            datasource: "main".into(),
            statement: "SELECT $1".into(),
            params: vec![SqlValue::Text("x".into()), SqlValue::Bool(true)],
        }),
        Request::Net(NetRequest::ConnectHost {
            host: "example.com".into(),
            port: 443,
        }),
        Request::Http(HttpRequest {
            method: "POST".into(),
            url: "https://a.example/x".into(),
            headers: vec![("x".into(), Bytes(vec![1]))],
            body: Bytes(vec![0, 255]),
            timeout_ms: Some(500),
        }),
        Request::Extension {
            package: "example:greeting".into(),
            function: "greet#hello".into(),
            args: Bytes(vec![0x81, 0x61, b'a']),
        },
    ]
}

fn answers() -> Vec<Answer> {
    vec![
        Answer::Unit,
        Answer::Pid(pid()),
        Answer::MaybePid(None),
        Answer::Ref(8),
        Answer::Called(Err(CallError::Exited(ExitReason::Timeout))),
        Answer::CastFailed(CastError::Overloaded),
        Answer::Registered(Err(RegisterError::Taken(pid()))),
        Answer::Kv(Ok(KvAnswer::Values(vec![Some(Bytes(vec![1])), None]))),
        Answer::KvOpened(Err(KvError::AccessDenied)),
        Answer::Groups(Ok(GroupsAnswer::Delivered(2))),
        Answer::Config(Err(ConfigError::AccessDenied)),
        Answer::ConfigAll(Ok(vec![("mode".into(), "fast".into())])),
        Answer::Secret(Err(SecretError::Unavailable("gone".into()))),
        Answer::Metric(Err(MetricError::AccessDenied)),
        Answer::Sql(Ok(SqlAnswer::Rows(SqlRows {
            columns: vec!["id".into()],
            rows: vec![vec![SqlValue::Int(1)], vec![SqlValue::Null]],
        }))),
        Answer::Net(Ok(NetAnswer::Stream(stream()))),
        Answer::Http(Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), Bytes(b"text/plain".to_vec()))],
            body: Bytes(vec![0, 255]),
        })),
        Answer::Extension(Err(ExtensionError::NoSuchFunction("greet#x".into()))),
    ]
}

/// Every sample as a line: its type, its name and its frame in hex. Info,
/// requests and answers travel inside `ToApp::Info`, `FromApp::Request`
/// and `ToApp::Answer`, so those are the frames encoded.
fn samples() -> Vec<(String, String, Vec<u8>)> {
    let mut out = Vec::new();
    for f in to_apps() {
        out.push(("ToApp".into(), to_app_name(&f).into(), protocol::encode(&f)));
    }
    for info in infos() {
        let name = info_name(&info).to_string();
        out.push(("Info".into(), name, protocol::encode(&ToApp::Info { info })));
    }
    for f in from_apps() {
        out.push((
            "FromApp".into(),
            from_app_name(&f).into(),
            protocol::encode(&f),
        ));
    }
    for request in requests() {
        let name = request_name(&request).to_string();
        out.push((
            "Request".into(),
            name,
            protocol::encode(&FromApp::Request { id: 2, request }),
        ));
    }
    for answer in answers() {
        let name = answer_name(&answer).to_string();
        out.push((
            "Answer".into(),
            name,
            protocol::encode(&ToApp::Answer { id: 2, answer }),
        ));
    }
    out.extend(hot_samples());
    out
}

/// The hot frames' fixed layouts.
fn hot_samples() -> Vec<(String, String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut push = |name: &str, bytes: Vec<u8>| out.push(("Hot".into(), name.into(), bytes));
    let mut b = Vec::new();
    hot::write_to_app(
        &mut b,
        &ToApp::Call {
            call_ref: 9,
            msg: env(),
        },
    )
    .unwrap();
    push("ToApp.Call", b);
    let mut b = Vec::new();
    hot::write_from_app(&mut b, &FromApp::Reply(Some(env()))).unwrap();
    push("FromApp.Reply", b);
    let mut b = Vec::new();
    hot::write_from_app(&mut b, &FromApp::Done).unwrap();
    push("FromApp.Done", b);
    let mut b = Vec::new();
    hot::write_kv_request(
        &mut b,
        &KvFrame {
            id: 3,
            bucket: 1,
            op: KvOp::Set,
            key: "k",
            value: b"v",
        },
    )
    .unwrap();
    push("Kv.Set", b);
    let mut b = Vec::new();
    hot::write_kv_answer(&mut b, 3, &KvReply::Value(Some(b"v")));
    push("KvAnswer.Value", b);
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// A file's lines: (type, name, frame).
fn read(version: u16) -> Vec<(String, String, Vec<u8>)> {
    let text = std::fs::read_to_string(file(version))
        .unwrap_or_else(|e| panic!("{}: {e}", file(version).display()));
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut parts = l.split(' ');
            let (ty, name, bytes) = (
                parts.next().unwrap(),
                parts.next().unwrap(),
                parts.next().unwrap(),
            );
            (ty.to_string(), name.to_string(), unhex(bytes))
        })
        .collect()
}

#[test]
fn samples____every_frame_named____one_each() {
    let found: BTreeSet<(String, String)> = samples()
        .into_iter()
        .filter(|(ty, ..)| ty != "Hot")
        .map(|(ty, name, _)| (ty, name))
        .collect();
    let expected: BTreeSet<(String, String)> = NAMES
        .iter()
        .flat_map(|(ty, names)| names.iter().map(|n| (ty.to_string(), n.to_string())))
        .collect();

    assert_eq!(found, expected);
}

#[test]
fn current_version____every_frame____encodes_to_its_golden_bytes() {
    let lines: Vec<String> = samples()
        .iter()
        .map(|(ty, name, bytes)| format!("{ty} {name} {}", hex(bytes)))
        .collect();
    if std::env::var_os("BULKHEAD_BLESS_PROTOCOL").is_some() {
        let header = format!(
            "# Protocol version {VERSION}: each frame's type, name and bytes (tests/protocol_versions.rs).\n"
        );
        std::fs::write(file(VERSION), header + &lines.join("\n") + "\n").unwrap();
    }

    let golden: Vec<String> = read(VERSION)
        .iter()
        .map(|(ty, name, bytes)| format!("{ty} {name} {}", hex(bytes)))
        .collect();

    assert_eq!(lines, golden);
}

#[test]
fn previous_version____every_frame____decodes_and_encodes_unchanged() {
    for (ty, name, frame) in read(MIN_VERSION) {
        let body = &frame[4..];
        let again = match ty.as_str() {
            "ToApp" | "Info" | "Answer" => {
                protocol::encode(&protocol::decode::<ToApp>(body).unwrap())
            }
            "FromApp" | "Request" => protocol::encode(&protocol::decode::<FromApp>(body).unwrap()),
            "Hot" => {
                let parsed = match name.split('.').next().unwrap() {
                    "ToApp" => hot::parse_to_app(body).map(|_| ()),
                    "FromApp" => hot::parse_from_app(body).map(|_| ()),
                    "Kv" => hot::parse_kv_request(body).map(|_| ()),
                    "KvAnswer" => hot::parse_kv_answer(body).map(|_| ()),
                    other => panic!("unknown hot frame {other}"),
                };
                parsed.unwrap_or_else(|e| panic!("{name}: {e}"));
                frame.clone()
            }
            other => panic!("unknown frame type {other}"),
        };

        assert_eq!(again, frame, "{ty} {name}");
    }
}
