//! Test fixture: the smallest app that implements the `paddock:runtime` app
//! world. It answers every call with the message it was sent and keeps no
//! state.
//!
//! Built for `wasm32-wasip2` (`cargo build -p paddock-fixture-echo --target
//! wasm32-wasip2 --release`); the host build only checks that the world's
//! bindings compile.

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

use bindings::{CallRef, CallResult, Envelope, ExitReason, Guest, Info};

/// The echo app.
struct Echo;

impl Guest for Echo {
    fn init(_args: Vec<u8>) -> Result<(), String> {
        Ok(())
    }

    fn handle_call(_caller: CallRef, msg: Envelope) -> CallResult {
        CallResult::Reply(msg)
    }

    fn handle_cast(_msg: Envelope) {}

    fn handle_info(_msg: Info) {}

    fn terminate(_reason: ExitReason) {}

    fn save_snapshot() -> Vec<u8> {
        Vec::new()
    }

    fn load_snapshot(_from_version: String, _snapshot: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
}

bindings::export!(Echo with_types_in bindings);
