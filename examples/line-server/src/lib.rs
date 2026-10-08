//! A connection handler written with `paddock-sdk`: each TCP connection on
//! the `lines` listener gets its own instance, which reads commands one per
//! line and answers each with a line.
//!
//! - `echo <text>`: reply `<text>`.
//! - `inc`, `get`: forwarded to the `counter` example app (found by its
//!   registered name), whose reply is passed back. The connection handles
//!   the protocol; the app keeps the state.
//! - `quit`: close the connection.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use paddock_sdk::connection::{Connection, ConnectionInfo, Stream};
use paddock_sdk::{Ctx, Envelope};

struct Lines;

fn counter(ctx: &mut Ctx, command: &str) -> String {
    let Some(pid) = ctx.whereis("counter") else {
        return "counter is not running".to_string();
    };
    match ctx.call(pid, Envelope::text(command), Duration::from_secs(1)) {
        Ok(reply) => reply.as_text().unwrap_or("").to_string(),
        Err(e) => format!("counter failed: {e}"),
    }
}

impl Connection for Lines {
    fn serve(ctx: &mut Ctx, _info: ConnectionInfo, stream: Stream) -> Result<(), String> {
        let mut out = stream.clone_writer();
        for line in BufReader::new(stream).lines() {
            let line = line.map_err(|e| e.to_string())?;
            let (verb, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            let reply = match verb {
                "echo" => rest.to_string(),
                "inc" | "get" => counter(ctx, verb),
                "quit" => return Ok(()),
                _ => "commands: echo <text>, inc, get, quit".to_string(),
            };
            writeln!(out, "{reply}").map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

paddock_sdk::connection!(Lines);
