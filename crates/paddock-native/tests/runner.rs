//! `paddock-runner` end to end, without a node: the test plays the node
//! over the runner's stdin and stdout.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use paddock_native::protocol::{
    self, Answer, Envelope, ExitReason, FromApp, Level, Pid, Request, ToApp, VERSION,
};

const ME: Pid = Pid {
    node: 1,
    id: 42,
    incarnation: 1,
};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The fixture executable and plugin, built on first use into their own
/// target directory (the `cargo test` running this holds the main one).
fn fixtures() -> &'static (PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let target = root().join("target/fixtures/native");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(root())
            .args([
                "build",
                "-p",
                "paddock-fixture-native",
                "-p",
                "paddock-fixture-plugin",
                "--target-dir",
            ])
            .arg(&target)
            .status()
            .expect("run cargo");
        assert!(status.success(), "building native fixtures failed");
        (
            target.join("debug/paddock-fixture-native"),
            target.join("debug/libpaddock_fixture_plugin.so"),
        )
    })
}

/// One runner, played as its node.
struct Port {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    logs: Arc<Mutex<Vec<(Level, String)>>>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Port {
    fn spawn(program: &str, path: &PathBuf) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_paddock-runner"))
            .arg(program)
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let lines = child.stderr.take().unwrap();
        let sink = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(lines).lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
        });
        let mut port = Self {
            stdin: child.stdin.take().unwrap(),
            stdout: child.stdout.take().unwrap(),
            child,
            logs: Arc::default(),
            stderr,
        };
        assert_eq!(port.next(), FromApp::Hello { version: VERSION });
        port
    }

    fn send(&mut self, frame: &ToApp) {
        protocol::write(&mut self.stdin, frame).unwrap();
    }

    /// The next frame that is not a log or a request; requests are
    /// answered as a node would for pid `ME`.
    fn next(&mut self) -> FromApp {
        loop {
            let frame = protocol::read::<FromApp>(&mut self.stdout)
                .unwrap()
                .unwrap_or_else(|| {
                    panic!(
                        "runner closed stdout; stderr: {:?}",
                        self.stderr.lock().unwrap()
                    )
                });
            match frame {
                FromApp::Log { level, message } => self.logs.lock().unwrap().push((level, message)),
                FromApp::Request { id, request } => {
                    let answer = match request {
                        Request::SelfPid => Answer::Pid(ME),
                        Request::Monitor(_) | Request::SendAfter { .. } => Answer::Ref(7),
                        _ => Answer::Unit,
                    };
                    self.send(&ToApp::Answer { id, answer });
                }
                other => return other,
            }
        }
    }

    fn init(&mut self, args: Vec<u8>) -> FromApp {
        self.send(&ToApp::Init { args });
        self.next()
    }

    fn call(&mut self, msg: Envelope) -> FromApp {
        self.send(&ToApp::Call { call_ref: 1, msg });
        self.next()
    }

    /// A text command's text reply.
    fn command(&mut self, command: &str) -> String {
        match self.call(Envelope::new("text/plain", command.as_bytes().to_vec())) {
            FromApp::Reply(Some(env)) => String::from_utf8(env.body).unwrap(),
            other => panic!("{command}: {other:?}"),
        }
    }

    fn exit_code(mut self) -> Option<i32> {
        drop(self.stdin);
        self.child.wait().unwrap().code()
    }
}

fn executable() -> Port {
    let mut port = Port::spawn("--executable", &fixtures().0);
    assert_eq!(port.init(Vec::new()), FromApp::Init(Ok(())));
    port
}

fn json(schema: &str, body: &str) -> Envelope {
    Envelope {
        content_type: "application/json".into(),
        schema: Some(schema.into()),
        body: body.as_bytes().to_vec(),
    }
}

#[test]
fn executable____calls_requests_and_terminate____served_in_order() {
    let mut port = executable();

    let echo = port.command("echo hello");
    let me = port.command("self");
    port.send(&ToApp::Terminate {
        reason: ExitReason::Shutdown,
    });
    let done = port.next();

    assert_eq!(echo, "hello");
    assert_eq!(me, "1.42.1");
    assert_eq!(done, FromApp::Done);
    assert_eq!(port.exit_code(), Some(0));
}

#[test]
fn executable____init_fails____error_and_exit() {
    let mut port = Port::spawn("--executable", &fixtures().0);

    let found = port.init(b"fail".to_vec());

    assert_eq!(found, FromApp::Init(Err("asked to fail".into())));
    assert_eq!(port.exit_code(), Some(1));
}

#[test]
fn executable____noreply_then_cast____reply_requested_later() {
    let mut port = executable();

    let first = port.call(Envelope::new("text/plain", b"noreply".to_vec()));
    port.send(&ToApp::Cast {
        msg: Envelope::new("text/plain", b"reply later".to_vec()),
    });
    let mut requested = None;
    let done = loop {
        match protocol::read::<FromApp>(&mut port.stdout)
            .unwrap()
            .unwrap()
        {
            FromApp::Request { id, request } => {
                requested = Some(request);
                port.send(&ToApp::Answer {
                    id,
                    answer: Answer::Unit,
                });
            }
            other => break other,
        }
    };

    assert_eq!(first, FromApp::Reply(None));
    assert_eq!(
        requested,
        Some(Request::Reply {
            call_ref: 1,
            msg: Envelope::new("text/plain", b"later".to_vec())
        })
    );
    assert_eq!(done, FromApp::Done);
}

#[test]
fn executable____panic____failed_with_message_and_exit() {
    let mut port = executable();

    let found = port.call(Envelope::new("text/plain", b"panic".to_vec()));

    assert_eq!(found, FromApp::Failed("panic: asked to".into()));
    assert_eq!(port.exit_code(), Some(101));
}

#[test]
fn executable____prints_and_logs____stderr_and_log_frames_not_frames_corrupted() {
    let mut port = executable();

    let printed = port.command("print to stdout");
    let logged = port.command("log careful");
    let after = port.command("echo still framed");
    let logs = port.logs.lock().unwrap().clone();
    port.send(&ToApp::Terminate {
        reason: ExitReason::Normal,
    });
    port.next();
    let stderr = port.stderr.clone();
    port.exit_code();

    assert_eq!((printed.as_str(), logged.as_str()), ("ok", "ok"));
    assert_eq!(after, "still framed");
    assert_eq!(logs, vec![(Level::Warn, "careful".to_string())]);
    assert!(stderr.lock().unwrap().iter().any(|l| l == "to stdout"));
}

#[test]
fn executable____sandboxed____files_exec_sockets_and_signals_refused() {
    let mut port = executable();
    let own = fixtures().0.display().to_string();
    let parent = std::process::id();
    let tmp = std::env::temp_dir().join(format!("paddock-native-{parent}"));

    let hostname = port.command("read /etc/hostname");
    let itself = port.command(&format!("read {own}"));
    let write = port.command(&format!("write {}", tmp.display()));
    let exec = port.command("exec");
    let connect = port.command("connect 127.0.0.1:9");
    let signal_parent = port.command(&format!("kill {parent}"));
    let signal_group = port.command("kill 0");

    assert!(hostname.contains("Permission denied"), "{hostname}");
    assert_eq!(itself, "ok");
    assert!(write.contains("Permission denied"), "{write}");
    assert!(!tmp.exists());
    assert!(exec.contains("Permission denied"), "{exec}");
    assert!(connect.contains("Operation not permitted"), "{connect}");
    assert!(
        signal_parent.contains("Operation not permitted"),
        "{signal_parent}"
    );
    assert!(
        signal_group.contains("Operation not permitted"),
        "{signal_group}"
    );
}

#[test]
fn library____json_calls____plugin_answers_with_schema() {
    let mut port = Port::spawn("--library", &fixtures().1);

    let init = port.init(br#"{"k":1}"#.to_vec());
    let echo = port.call(json("echo", r#"{"a":[1,2]}"#));
    let config = port.call(json("config", "{}"));
    let fail = port.call(json("fail", "{}"));
    let text = port.call(Envelope::new("text/plain", b"hi".to_vec()));
    let read = port.call(json("read", r#"{"path":"/etc/hostname"}"#));
    let logs = port.logs.lock().unwrap().clone();

    assert_eq!(init, FromApp::Init(Ok(())));
    assert_eq!(echo, FromApp::Reply(Some(json("echo", r#"{"a":[1,2]}"#))));
    assert_eq!(config, FromApp::Reply(Some(json("config", r#"{"k":1}"#))));
    for (what, found) in [("fail", &fail), ("text", &text), ("read", &read)] {
        let FromApp::Reply(Some(env)) = found else {
            panic!("{what}: {found:?}");
        };
        assert_eq!(env.schema.as_deref(), Some("rustbridge.error"), "{what}");
    }
    let FromApp::Reply(Some(read)) = read else {
        unreachable!()
    };
    assert!(String::from_utf8_lossy(&read.body).contains("Permission denied"));
    assert!(
        logs.iter()
            .any(|(_, m)| m.contains("fixture plugin started")),
        "{logs:?}"
    );
}
