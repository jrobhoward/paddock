//! A connection handler written with `bulkhead-sdk`, speaking a line
//! protocol on the `lines` listener: one command per line, a line back for
//! each. Each instance holds many connections.
//!
//! - `echo <text>`: reply `<text>`.
//! - `inc`, `get`: forwarded to the `counter` example app (found by its
//!   registered name), whose reply is passed back. The connection handles
//!   the protocol; the app keeps the state.
//! - `quit`: close the connection.

use std::io::Write;
use std::time::Duration;

use bulkhead_sdk::connection::{ConnectionInfo, Flow, Handler, Output};
use bulkhead_sdk::{Ctx, Envelope};

/// One connection: the bytes of a line not yet complete.
struct Lines {
    pending: Vec<u8>,
}

fn counter(ctx: &mut Ctx, command: &str) -> String {
    let Some(pid) = ctx.whereis("counter") else {
        return "counter is not running".to_string();
    };
    match ctx.call(pid, Envelope::text(command), Duration::from_secs(1)) {
        Ok(reply) => reply.as_text().unwrap_or("").to_string(),
        Err(e) => format!("counter failed: {e}"),
    }
}

impl Handler for Lines {
    fn open(_ctx: &mut Ctx, _info: &ConnectionInfo) -> Result<Self, String> {
        Ok(Self {
            pending: Vec::new(),
        })
    }

    fn data(&mut self, ctx: &mut Ctx, bytes: &[u8], out: &mut Output<'_>) -> Result<Flow, String> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end();
            let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
            let reply = match verb {
                "echo" => rest.to_string(),
                "inc" | "get" => counter(ctx, verb),
                "quit" => return Ok(Flow::Close),
                _ => "commands: echo <text>, inc, get, quit".to_string(),
            };
            writeln!(out, "{reply}").map_err(|e| e.to_string())?;
        }
        Ok(Flow::Continue)
    }
}

bulkhead_sdk::connection_handler!(Lines);
