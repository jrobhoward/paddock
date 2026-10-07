//! Arbitrary sequences of messages, process operations and pids against live
//! instances of the chaos fixture: calls and casts with arbitrary
//! envelopes, links, monitors, exit signals, registration and timers between
//! instances, kills, and messages to made-up pids. Afterwards every instance
//! must die when killed, and a fresh instance must still answer.
//!
//! Needs the fixture built first (or `PADDOCK_FUZZ_COMPONENT` set):
//! `cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos --target-dir target/fixtures`

#![no_main]

use std::sync::OnceLock;
use std::time::Duration;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use paddock_host::{Envelope, InstanceHandle, InstanceSpec, LoadedApp, Pid, Runtime, RuntimeConfig};

const SLOTS: usize = 4;
const SHORT: Duration = Duration::from_millis(50);

#[derive(Debug, Arbitrary)]
enum Target {
    /// One of the live instances.
    Slot(u8),
    /// A made-up pid, live or not.
    Raw { node: u32, id: u64, incarnation: u32 },
}

#[derive(Debug, Arbitrary)]
enum Command {
    Echo(String),
    Mirror,
    Count,
    Trap,
    NoReply,
    Defer,
    Answer(String),
    AnswerNow(String),
    SelfPid,
    Call { to: Target, timeout_ms: u8, inner: Box<Command> },
    CallSelf,
    Cast { to: Target, inner: Box<Command> },
    Register(String),
    Whereis(String),
    Timer { ms: u8, text: String },
    Cancel(u64),
    Timers,
    Link(Target),
    Unlink(Target),
    TrapExits(bool),
    Monitor(Target),
    Demonitor(u64),
    Exit { to: Target, reason: u8, text: String },
    Infos,
    Alloc(u8),
    /// Bytes that are not a command at all.
    Garbage(Vec<u8>),
}

#[derive(Debug, Arbitrary)]
enum Op {
    Call { to: Target, content_type: String, command: Command },
    Cast { to: Target, content_type: String, command: Command },
    Kill(u8),
    Respawn(u8),
}

fn runtime() -> &'static (tokio::runtime::Runtime, Runtime, LoadedApp) {
    static RT: OnceLock<(tokio::runtime::Runtime, Runtime, LoadedApp)> = OnceLock::new();
    RT.get_or_init(|| {
        let path = std::env::var("PADDOCK_FUZZ_COMPONENT").unwrap_or_else(|_| {
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/fixtures/wasm32-wasip2/release/paddock_fixture_chaos.wasm"
            )
            .to_string()
        });
        let component = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let rt = tokio.block_on(async {
            Runtime::new(RuntimeConfig { max_instances: 64, ..RuntimeConfig::default() }).unwrap()
        });
        let app = rt.load(&component).unwrap();
        (tokio, rt, app)
    })
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        memory_max: 16 << 20,
        callback_deadline: Duration::from_millis(200),
        mailbox_max: 8,
        message_max_bytes: 64 << 10,
        init_timeout: Duration::from_millis(500),
        shutdown_timeout: Duration::from_millis(100),
        registered_names: vec!["a".into(), "b".into()],
        ..InstanceSpec::new("fuzz", "0.1.0")
    }
}

fn show(pid: Pid) -> String {
    format!("{}.{}.{}", pid.node, pid.id, pid.incarnation)
}

fn resolve(target: &Target, slots: &[InstanceHandle]) -> Pid {
    match target {
        Target::Slot(i) => slots[usize::from(*i) % slots.len()].pid(),
        Target::Raw { node, id, incarnation } => Pid { node: *node, id: *id, incarnation: *incarnation },
    }
}

fn render(command: &Command, slots: &[InstanceHandle]) -> Vec<u8> {
    let pid = |t: &Target| show(resolve(t, slots));
    let text = match command {
        Command::Echo(s) => format!("echo:{s}"),
        Command::Mirror => "mirror".into(),
        Command::Count => "count".into(),
        Command::Trap => "trap".into(),
        Command::NoReply => "noreply".into(),
        Command::Defer => "defer".into(),
        Command::Answer(s) => format!("answer:{s}"),
        Command::AnswerNow(s) => format!("answer-now:{s}"),
        Command::SelfPid => "self".into(),
        Command::Call { to, timeout_ms, inner } => format!(
            "call:{}:{}:{}",
            pid(to),
            timeout_ms % 50,
            String::from_utf8_lossy(&render(inner, slots))
        ),
        Command::CallSelf => "call-self".into(),
        Command::Cast { to, inner } => {
            format!("cast:{}:{}", pid(to), String::from_utf8_lossy(&render(inner, slots)))
        }
        Command::Register(name) => format!("register:{name}"),
        Command::Whereis(name) => format!("whereis:{name}"),
        Command::Timer { ms, text } => format!("timer:{}:{text}", ms % 50),
        Command::Cancel(r) => format!("cancel:{r}"),
        Command::Timers => "timers".into(),
        Command::Link(t) => format!("link:{}", pid(t)),
        Command::Unlink(t) => format!("unlink:{}", pid(t)),
        Command::TrapExits(on) => format!("trap-exits:{}", if *on { "on" } else { "off" }),
        Command::Monitor(t) => format!("monitor:{}", pid(t)),
        Command::Demonitor(r) => format!("demonitor:{r}"),
        Command::Exit { to, reason, text } => {
            let reason = match reason % 4 {
                0 => "normal".to_string(),
                1 => "shutdown".to_string(),
                2 => "killed".to_string(),
                _ => format!("crashed-{text}"),
            };
            format!("exit:{}:{reason}", pid(to))
        }
        Command::Infos => "infos".into(),
        // Up to 24 MiB against a 16 MiB cap.
        Command::Alloc(mib) => format!("alloc:{}", mib % 25),
        Command::Garbage(bytes) => return bytes.clone(),
    };
    text.into_bytes()
}

fuzz_target!(|ops: Vec<Op>| {
    let (tokio, rt, app) = runtime();
    tokio.block_on(async {
        let mut slots: Vec<InstanceHandle> = (0..SLOTS).map(|_| rt.spawn(app, spec()).unwrap()).collect();
        for op in ops.iter().take(64) {
            match op {
                Op::Call { to, content_type, command } => {
                    let msg = Envelope::new(content_type.clone(), render(command, &slots));
                    let _ = rt.call(resolve(to, &slots), msg, SHORT).await;
                }
                Op::Cast { to, content_type, command } => {
                    let msg = Envelope::new(content_type.clone(), render(command, &slots));
                    let _ = rt.cast(resolve(to, &slots), msg);
                }
                Op::Kill(i) => slots[usize::from(*i) % SLOTS].kill(),
                Op::Respawn(i) => {
                    let i = usize::from(*i) % SLOTS;
                    slots[i].kill();
                    slots[i] = rt.spawn(app, spec()).unwrap();
                }
            }
        }
        for slot in &slots {
            slot.kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), slot.wait())
                .await
                .expect("a killed instance exits");
        }
        let fresh = rt.spawn(app, spec()).unwrap();
        let reply = fresh
            .call(Envelope::new("text/plain", "echo:alive"), Duration::from_secs(5))
            .await
            .expect("a fresh instance answers");
        assert_eq!(reply.body, b"alive");
        fresh.kill();
        let _ = fresh.wait().await;
    });
});
