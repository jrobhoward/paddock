//! Test fixture: an app that misbehaves on request, for testing how the host
//! maps failures to exit reasons.
//!
//! `init` arguments are UTF-8 words separated by spaces:
//! - `fail`: `init` returns an error.
//! - `trap`: `init` panics.
//! - `spin`: `init` loops forever.
//! - `terminate-spin`: `terminate` loops forever.
//!
//! Message bodies (calls and casts) are UTF-8 commands:
//! - `echo:<text>`: reply `<text>`.
//! - `count`: add one to a counter and reply its value.
//! - `trap`: panic.
//! - `spin`: loop forever.
//! - `alloc:<mib>`: allocate and touch `<mib>` MiB, keep it, reply `ok`.
//! - `noreply`: return `noreply` and never answer.
//! - `self`: reply this instance's pid as `node.id.incarnation`.
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
use bindings::{CallRef, CallResult, Envelope, ExitReason, Guest, Info};

#[derive(Default)]
struct State {
    count: u64,
    terminate_spin: bool,
    hoard: Vec<Vec<u8>>,
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

fn reply(text: impl Into<String>) -> Envelope {
    Envelope {
        content_type: "text/plain".to_string(),
        schema: None,
        body: text.into().into_bytes(),
    }
}

/// Run one command; `None` means no reply.
fn run(body: &[u8]) -> Option<String> {
    let command = String::from_utf8_lossy(body);
    let (verb, arg) = command.split_once(':').unwrap_or((&command, ""));
    match verb {
        "echo" => Some(arg.to_string()),
        "count" => Some(STATE.with_borrow_mut(|s| {
            s.count += 1;
            s.count.to_string()
        })),
        "trap" => panic!("asked to trap"),
        "spin" => spin(),
        "alloc" => {
            let mib: usize = arg.parse().unwrap_or(0);
            let block = vec![1u8; mib << 20];
            STATE.with_borrow_mut(|s| s.hoard.push(std::hint::black_box(block)));
            Some("ok".to_string())
        }
        "noreply" => None,
        "self" => {
            let pid = process::self_();
            Some(format!("{}.{}.{}", pid.node, pid.id, pid.incarnation))
        }
        _ => Some(format!("unknown command '{command}'")),
    }
}

impl Guest for Chaos {
    fn init(args: Vec<u8>) -> Result<(), String> {
        let args = String::from_utf8_lossy(&args).into_owned();
        for word in args.split_whitespace() {
            match word {
                "fail" => return Err("asked to fail".to_string()),
                "trap" => panic!("asked to trap in init"),
                "spin" => spin(),
                "terminate-spin" => STATE.with_borrow_mut(|s| s.terminate_spin = true),
                _ => {}
            }
        }
        Ok(())
    }

    fn handle_call(_caller: CallRef, msg: Envelope) -> CallResult {
        match run(&msg.body) {
            Some(text) => CallResult::Reply(reply(text)),
            None => CallResult::Noreply,
        }
    }

    fn handle_cast(msg: Envelope) {
        run(&msg.body);
    }

    fn handle_info(_msg: Info) {}

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
