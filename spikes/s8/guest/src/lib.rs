//! A Paddock app that runs the S8 kernels on request.

#[allow(missing_docs, clippy::too_many_arguments)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "paddock:runtime/app@0.1.0",
        pub_export_macro: true,
    });
}

use bindings::{CallRef, CallResult, Envelope, ExitReason, Guest, Info};

struct Bench;

impl Guest for Bench {
    fn init(_args: Vec<u8>) -> Result<(), String> {
        Ok(())
    }

    fn handle_call(_caller: CallRef, msg: Envelope) -> CallResult {
        // Transfer tests: the body is a command prefix plus payload bytes.
        if msg.body.starts_with(b"len:") {
            return CallResult::Reply(Envelope {
                content_type: "text/plain".into(),
                schema: None,
                body: (msg.body.len() - 4).to_string().into_bytes(),
            });
        }
        if msg.body.starts_with(b"mirror:") {
            return CallResult::Reply(msg);
        }
        let command = String::from_utf8_lossy(&msg.body);
        CallResult::Reply(Envelope {
            content_type: "text/plain".into(),
            schema: None,
            body: s8_kernels::run(&command).into_bytes(),
        })
    }

    fn handle_cast(_msg: Envelope) {}
    fn handle_info(_msg: Info) {}
    fn terminate(_reason: ExitReason) {}
    fn save_snapshot() -> Vec<u8> {
        Vec::new()
    }
    fn load_snapshot(_from: String, _snapshot: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
}

bindings::export!(Bench with_types_in bindings);
