//! Test fixture: an app that misbehaves on request, and drives every
//! `process` function, for testing the host.
//!
//! `init` arguments are UTF-8 words separated by spaces, optionally inside a
//! JSON string (as manifest `start_args` arrive):
//! - `fail`: `init` returns an error.
//! - `trap`: `init` panics.
//! - `spin`: `init` loops forever.
//! - `terminate-spin`: `terminate` loops forever.
//! - `register:<name>`: register `<name>` during `init`.
//! - `trap-exits`: trap exits from `init` on.
//!
//! Message bodies (calls and casts) are UTF-8 commands. Pids are written
//! `node.id.incarnation`.
//! - `echo:<text>`: reply `<text>`.
//! - `mirror`: reply the received envelope unchanged.
//! - `count`: add one to a counter and reply its value.
//! - `trap`: panic.
//! - `spin`: loop forever.
//! - `alloc:<mib>`: allocate and touch `<mib>` MiB, keep it, reply `ok`.
//! - `noreply`: return `noreply` and never answer.
//! - `defer`: return `noreply` and remember the caller.
//! - `answer:<text>`: reply `<text>` to every remembered caller.
//! - `answer-now:<text>`: reply `<text>` to this call with `process.reply`,
//!   then return `noreply`.
//! - `self`: reply this instance's pid.
//! - `call:<pid>:<timeout-ms>:<command>`: call `<pid>` with `<command>`;
//!   reply `ok:<body>` or `err:<error>`.
//! - `call-self`: call this instance's own pid.
//! - `cast:<pid>:<command>`: cast `<command>` to `<pid>`; reply `sent`.
//! - `register:<name>`: reply `ok` or `err:<error>`.
//! - `whereis:<name>`: reply the pid or `none`.
//! - `timer:<ms>:<text>`: start a timer; reply its reference.
//! - `cancel:<ref>`: cancel a timer; reply `cancelled`.
//! - `timers`: reply the texts of the timers that fired, comma-separated.
//! - `link:<pid>` / `unlink:<pid>`: reply `ok`.
//! - `trap-exits:<on|off>`: reply `ok`.
//! - `monitor:<pid>`: reply the monitor reference.
//! - `demonitor:<ref>`: reply `ok`.
//! - `exit:<pid>:<reason>`: send an exit signal; reasons are `normal`,
//!   `shutdown`, `killed` or `crashed-<text>`. Reply `ok`.
//! - `infos`: reply the exit and down notifications received, as
//!   `exit:<pid>:<reason>` and `down:<ref>:<pid>:<reason>`, joined by `;`.
//!
//! Built for `wasm32-wasip2`; the host build only checks that the bindings
//! compile.

// Generated bindings carry no doc comments, and their ABI glue flattens an
// envelope into more core arguments than clippy's limit.
#[allow(missing_docs, clippy::too_many_arguments)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "paddock:runtime/app@0.1.0",
        pub_export_macro: true,
    });
}

use std::cell::RefCell;

use bindings::paddock::runtime::process;
use bindings::paddock::runtime::types::{CallError, Pid, RegisterError};
use bindings::{CallRef, CallResult, Envelope, ExitReason, Guest, Info};

#[derive(Default)]
struct State {
    count: u64,
    terminate_spin: bool,
    hoard: Vec<Vec<u8>>,
    deferred: Vec<CallRef>,
    fired: Vec<String>,
    infos: Vec<String>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// The chaos app.
struct Chaos;

fn spin() -> ! {
    let mut n = 0u64;
    loop {
        n = std::hint::black_box(n.wrapping_add(1));
    }
}

fn text(body: impl Into<String>) -> Envelope {
    Envelope {
        content_type: "text/plain".to_string(),
        schema: None,
        body: body.into().into_bytes(),
    }
}

fn show_pid(pid: Pid) -> String {
    format!("{}.{}.{}", pid.node, pid.id, pid.incarnation)
}

fn parse_pid(s: &str) -> Pid {
    let mut parts = s.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let mut next = || parts.next().unwrap_or(0);
    Pid {
        node: next() as u32,
        id: next(),
        incarnation: next() as u32,
    }
}

fn show_exit(reason: &ExitReason) -> String {
    match reason {
        ExitReason::Normal => "normal".into(),
        ExitReason::Shutdown => "shutdown".into(),
        ExitReason::Killed => "killed".into(),
        ExitReason::Noproc => "noproc".into(),
        ExitReason::Noconnection => "noconnection".into(),
        ExitReason::Timeout => "timeout".into(),
        ExitReason::Crashed(why) => format!("crashed({why})"),
    }
}

fn show_call_error(err: &CallError) -> String {
    match err {
        CallError::Noproc => "noproc".into(),
        CallError::Noconnection => "noconnection".into(),
        CallError::Timeout => "timeout".into(),
        CallError::Overloaded => "overloaded".into(),
        CallError::TooLarge => "too-large".into(),
        CallError::AccessDenied => "access-denied".into(),
        CallError::Exited(reason) => format!("exited:{}", show_exit(reason)),
    }
}

fn parse_exit(s: &str) -> ExitReason {
    match s {
        "normal" => ExitReason::Normal,
        "shutdown" => ExitReason::Shutdown,
        "killed" => ExitReason::Killed,
        other => ExitReason::Crashed(other.strip_prefix("crashed-").unwrap_or(other).to_string()),
    }
}

fn register(name: &str) -> String {
    match process::register(name) {
        Ok(()) => "ok".into(),
        Err(RegisterError::AccessDenied) => "err:access-denied".into(),
        Err(RegisterError::Taken(pid)) => format!("err:taken:{}", show_pid(pid)),
    }
}

/// What a command does with the call.
enum Outcome {
    Reply(Envelope),
    NoReply,
}

fn run(caller: Option<CallRef>, msg: &Envelope) -> Outcome {
    let command = String::from_utf8_lossy(&msg.body).into_owned();
    let (verb, arg) = command.split_once(':').unwrap_or((&command, ""));
    let reply = |s: String| Outcome::Reply(text(s));
    match verb {
        "echo" => reply(arg.to_string()),
        "mirror" => Outcome::Reply(msg.clone()),
        "count" => reply(STATE.with_borrow_mut(|s| {
            s.count += 1;
            s.count.to_string()
        })),
        "trap" => panic!("asked to trap"),
        "spin" => spin(),
        "alloc" => {
            let mib: usize = arg.parse().unwrap_or(0);
            let block = vec![1u8; mib << 20];
            STATE.with_borrow_mut(|s| s.hoard.push(std::hint::black_box(block)));
            reply("ok".into())
        }
        "noreply" => Outcome::NoReply,
        "defer" => {
            if let Some(caller) = caller {
                STATE.with_borrow_mut(|s| s.deferred.push(caller));
            }
            Outcome::NoReply
        }
        "answer" => {
            for caller in STATE.with_borrow_mut(|s| std::mem::take(&mut s.deferred)) {
                process::reply(caller, &text(arg));
            }
            reply("answered".into())
        }
        "answer-now" => {
            if let Some(caller) = caller {
                process::reply(caller, &text(arg));
            }
            Outcome::NoReply
        }
        "self" => reply(show_pid(process::self_())),
        "call" => {
            let mut parts = arg.splitn(3, ':');
            let pid = parse_pid(parts.next().unwrap_or(""));
            let timeout: u32 = parts.next().unwrap_or("1000").parse().unwrap_or(1000);
            let inner = text(parts.next().unwrap_or(""));
            reply(match process::call(pid, &inner, timeout) {
                Ok(env) => format!("ok:{}", String::from_utf8_lossy(&env.body)),
                Err(err) => format!("err:{}", show_call_error(&err)),
            })
        }
        "call-self" => {
            let result = process::call(process::self_(), &text("echo:me"), 1000);
            reply(format!("{}", result.is_ok()))
        }
        "cast" => {
            let (pid, inner) = arg.split_once(':').unwrap_or((arg, ""));
            process::cast(parse_pid(pid), &text(inner));
            reply("sent".into())
        }
        "register" => reply(register(arg)),
        "whereis" => reply(process::whereis(arg).map_or("none".into(), show_pid)),
        "timer" => {
            let (ms, body) = arg.split_once(':').unwrap_or((arg, ""));
            let reference = process::send_after(ms.parse().unwrap_or(0), &text(body));
            reply(reference.to_string())
        }
        "cancel" => {
            process::cancel_timer(arg.parse().unwrap_or(0));
            reply("cancelled".into())
        }
        "timers" => reply(STATE.with_borrow(|s| s.fired.join(","))),
        "link" => {
            process::link(parse_pid(arg));
            reply("ok".into())
        }
        "unlink" => {
            process::unlink(parse_pid(arg));
            reply("ok".into())
        }
        "trap-exits" => {
            process::trap_exits(arg == "on");
            reply("ok".into())
        }
        "monitor" => reply(process::monitor(parse_pid(arg)).to_string()),
        "demonitor" => {
            process::demonitor(arg.parse().unwrap_or(0));
            reply("ok".into())
        }
        "exit" => {
            let (pid, reason) = arg.split_once(':').unwrap_or((arg, "normal"));
            process::exit(parse_pid(pid), &parse_exit(reason));
            reply("ok".into())
        }
        "infos" => reply(STATE.with_borrow(|s| s.infos.join(";"))),
        _ => reply(format!("unknown command '{command}'")),
    }
}

impl Guest for Chaos {
    fn init(args: Vec<u8>) -> Result<(), String> {
        let args = String::from_utf8_lossy(&args).into_owned();
        // Manifest `start_args` arrive as JSON; accept a JSON string too.
        let args = args.trim().trim_matches('"');
        for word in args.split_whitespace() {
            match word.split_once(':') {
                Some(("register", name)) => {
                    let result = register(name);
                    if result != "ok" {
                        return Err(result);
                    }
                }
                _ => match word {
                    "fail" => return Err("asked to fail".to_string()),
                    "trap" => panic!("asked to trap in init"),
                    "spin" => spin(),
                    "terminate-spin" => STATE.with_borrow_mut(|s| s.terminate_spin = true),
                    "trap-exits" => process::trap_exits(true),
                    _ => {}
                },
            }
        }
        Ok(())
    }

    fn handle_call(caller: CallRef, msg: Envelope) -> CallResult {
        match run(Some(caller), &msg) {
            Outcome::Reply(env) => CallResult::Reply(env),
            Outcome::NoReply => CallResult::Noreply,
        }
    }

    fn handle_cast(msg: Envelope) {
        run(None, &msg);
    }

    fn handle_info(msg: Info) {
        match msg {
            Info::Timer((_, envelope)) => {
                let body = String::from_utf8_lossy(&envelope.body).into_owned();
                STATE.with_borrow_mut(|s| s.fired.push(body));
            }
            Info::Exit((pid, reason)) => {
                let line = format!("exit:{}:{}", show_pid(pid), show_exit(&reason));
                STATE.with_borrow_mut(|s| s.infos.push(line));
            }
            Info::HostEvent((name, _)) => {
                STATE.with_borrow_mut(|s| s.infos.push(format!("event:{name}")));
            }
            Info::Down((reference, pid, reason)) => {
                let line = format!("down:{reference}:{}:{}", show_pid(pid), show_exit(&reason));
                STATE.with_borrow_mut(|s| s.infos.push(line));
            }
        }
    }

    fn terminate(_reason: ExitReason) {
        if STATE.with_borrow(|s| s.terminate_spin) {
            spin();
        }
    }

    /// The snapshot is the count, then each remembered caller, as 8-byte
    /// little-endian numbers, so a new version can answer deferred calls.
    fn save_snapshot() -> Vec<u8> {
        STATE.with_borrow(|s| {
            let mut out = s.count.to_le_bytes().to_vec();
            for caller in &s.deferred {
                out.extend_from_slice(&caller.to_le_bytes());
            }
            out
        })
    }

    fn load_snapshot(_from_version: String, snapshot: Vec<u8>) -> Result<(), String> {
        if snapshot.len() < 8 || !snapshot.len().is_multiple_of(8) {
            return Err("snapshot must be whole 8-byte numbers".to_string());
        }
        let mut numbers = snapshot
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c));
        let count = numbers.next().unwrap_or(0);
        let deferred: Vec<CallRef> = numbers.collect();
        STATE.with_borrow_mut(|s| {
            s.count = count;
            s.deferred = deferred;
        });
        Ok(())
    }
}

bindings::export!(Chaos with_types_in bindings);
