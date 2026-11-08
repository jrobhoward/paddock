//! An app that calls the `example:greeting` extension, written with
//! `bulkhead-sdk` plus a client module for the extension's package
//! ([`greeting`]).
//!
//! Commands: `hello:<name>` (the greeting) and `count` (greetings made on
//! this node). Errors reply `err:<error>`.

use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};

pub mod greeting;

use greeting as greet;

#[derive(Default)]
struct Greeted;

fn show(error: &greet::Error) -> String {
    match error {
        greet::Error::AccessDenied => "access-denied".into(),
        greet::Error::Other(e) => e.clone(),
    }
}

impl App for Greeted {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self)
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let reply = match text.split_once(':') {
            Some(("hello", name)) => {
                greet::hello(name).unwrap_or_else(|e| format!("err:{}", show(&e)))
            }
            _ if text == "count" => {
                greet::count().map_or_else(|e| format!("err:{}", show(&e)), |n| n.to_string())
            }
            _ => "commands: hello:<name>, count".into(),
        };
        Reply::Now(Envelope::text(reply))
    }
}

bulkhead_sdk::app!(Greeted);
