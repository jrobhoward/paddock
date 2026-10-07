//! Test fixture: a stateful counter in three versions, for upgrade tests.
//!
//! - v1 (no features) keeps a count. Its snapshot is the count in decimal.
//! - v2 (`--features v2`) also keeps the last three values. Its snapshot is
//!   `count;h1,h2,h3`. It reads v1 and v2 snapshots.
//! - v3 (`--features v3`) is v2 whose `load-snapshot` always fails.
//!
//! `init` arguments: `register:<name>` registers the name.
//!
//! Commands: `inc` (reply the new count), `get` (reply
//! `<version> count=<n>` and, from v2 on, ` history=<h1,h2,...>`), `self`
//! (reply the pid), `trap` (panic).

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

#[cfg(not(any(feature = "v2", feature = "v3")))]
const VERSION: &str = "1.0.0";
#[cfg(all(feature = "v2", not(feature = "v3")))]
const VERSION: &str = "2.0.0";
#[cfg(feature = "v3")]
const VERSION: &str = "3.0.0";

const HISTORY: bool = cfg!(any(feature = "v2", feature = "v3"));

#[derive(Default)]
struct State {
    count: u64,
    history: Vec<u64>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// The counter app.
struct Counter;

fn text(body: String) -> Envelope {
    Envelope {
        content_type: "text/plain".into(),
        schema: None,
        body: body.into_bytes(),
    }
}

fn describe(s: &State) -> String {
    if HISTORY {
        let h: Vec<String> = s.history.iter().map(u64::to_string).collect();
        format!("{VERSION} count={} history={}", s.count, h.join(","))
    } else {
        format!("{VERSION} count={}", s.count)
    }
}

impl Guest for Counter {
    fn init(args: Vec<u8>) -> Result<(), String> {
        let args = String::from_utf8_lossy(&args).into_owned();
        for word in args.trim().trim_matches('"').split_whitespace() {
            if let Some(name) = word.strip_prefix("register:") {
                process::register(name).map_err(|_| format!("cannot register {name}"))?;
            }
        }
        Ok(())
    }

    fn handle_call(_caller: CallRef, msg: Envelope) -> CallResult {
        let command = String::from_utf8_lossy(&msg.body).into_owned();
        let reply = match command.as_str() {
            "inc" => STATE.with_borrow_mut(|s| {
                s.count += 1;
                if HISTORY {
                    s.history.push(s.count);
                    if s.history.len() > 3 {
                        s.history.remove(0);
                    }
                }
                s.count.to_string()
            }),
            "get" => STATE.with_borrow(describe),
            "self" => {
                let p = process::self_();
                format!("{}.{}.{}", p.node, p.id, p.incarnation)
            }
            "trap" => panic!("asked to trap"),
            other => format!("unknown command '{other}'"),
        };
        CallResult::Reply(text(reply))
    }

    fn handle_cast(_msg: Envelope) {}
    fn handle_info(_msg: Info) {}
    fn terminate(_reason: ExitReason) {}

    fn save_snapshot() -> Vec<u8> {
        STATE.with_borrow(|s| {
            if HISTORY {
                let h: Vec<String> = s.history.iter().map(u64::to_string).collect();
                format!("{};{}", s.count, h.join(","))
            } else {
                s.count.to_string()
            }
            .into_bytes()
        })
    }

    fn load_snapshot(from_version: String, snapshot: Vec<u8>) -> Result<(), String> {
        if cfg!(feature = "v3") {
            return Err(format!("v3 cannot read snapshots from {from_version}"));
        }
        let text = String::from_utf8(snapshot).map_err(|e| e.to_string())?;
        let (count, history) = text.split_once(';').unwrap_or((&text, ""));
        let count: u64 = count
            .parse()
            .map_err(|_| format!("bad count in '{text}'"))?;
        let history: Vec<u64> = if HISTORY {
            history.split(',').filter_map(|h| h.parse().ok()).collect()
        } else {
            Vec::new()
        };
        STATE.with_borrow_mut(|s| {
            s.count = count;
            s.history = history;
        });
        Ok(())
    }
}

bindings::export!(Counter with_types_in bindings);
