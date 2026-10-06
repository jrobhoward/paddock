//! Test fixture: an app that misbehaves on request, and drives every
//! `process` function, for testing the host.
//!
//! `init` arguments are UTF-8 words separated by spaces:
//! - `fail`: `init` returns an error.
//! - `trap`: `init` panics.
//! - `spin`: `init` loops forever.
//! - `terminate-spin`: `terminate` loops forever.
//! - `register:<name>`: register `<name>` during `init`.
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
        _ => reply(format!("unknown command '{command}'")),
    }
}

impl Guest for Chaos {
    fn init(args: Vec<u8>) -> Result<(), String> {
        let args = String::from_utf8_lossy(&args).into_owned();
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
        if let Info::Timer((_, envelope)) = msg {
            let body = String::from_utf8_lossy(&envelope.body).into_owned();
            STATE.with_borrow_mut(|s| s.fired.push(body));
        }
    }

    fn terminate(_reason: ExitReason) {
        if STATE.with_borrow(|s| s.terminate_spin) {
            spin();
        }
    }

    fn save_snapshot() -> Vec<u8> {
        STATE.with_borrow(|s| s.count.to_le_bytes().to_vec())
    }

    fn load_snapshot(_from_version: String, snapshot: Vec<u8>) -> Result<(), String> {
        let bytes: [u8; 8] = snapshot
            .try_into()
            .map_err(|_| "snapshot must be 8 bytes".to_string())?;
        STATE.with_borrow_mut(|s| s.count = u64::from_le_bytes(bytes));
        Ok(())
    }
}

bindings::export!(Chaos with_types_in bindings);
