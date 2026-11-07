//! Test fixture: a connection handler on `bulkhead-sdk`'s
//! `connection::Handler`, built as a component (`bulkhead:connection`) and,
//! with the `native` feature, as a native program. A line protocol, one
//! command per line, one reply line each:
//!
//! - `echo <text>`: reply `<text>` (`v2:<text>` with the `v2` feature).
//! - `info`: reply `<listener> <peer>`.
//! - `self`: reply this handler's pid.
//! - `call <name> <body>`: call the instance registered as `<name>` with a
//!   text body; reply its reply, or `err:<error>`.
//! - `quit`: close the connection normally.
//! - `error`: return an error (a component's crash with a reason; natively
//!   this connection closes).
//! - `crash`: panic (a trap; natively the instance crashes).
//! - `spin`: loop forever without I/O (a component's idle timeout ends it;
//!   natively the callback deadline).

use std::io::Write;
use std::time::Duration;

use bulkhead_sdk::connection::{ConnectionInfo, Flow, Handler, Output};
use bulkhead_sdk::{Ctx, Envelope};

struct Lines {
    info: ConnectionInfo,
    pending: Vec<u8>,
}

fn command(ctx: &mut Ctx, info: &ConnectionInfo, line: &str) -> Result<Option<String>, String> {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    Ok(Some(match verb {
        "echo" if cfg!(feature = "v2") => format!("v2:{rest}"),
        "echo" => rest.to_string(),
        "info" => format!("{} {}", info.listener, info.peer),
        "self" => ctx.pid().to_string(),
        "call" => {
            let (name, body) = rest.split_once(' ').unwrap_or((rest, ""));
            match ctx.whereis(name) {
                None => "err:noproc".to_string(),
                Some(pid) => match ctx.call(pid, Envelope::text(body), Duration::from_secs(1)) {
                    Ok(reply) => reply.as_text().unwrap_or("").to_string(),
                    Err(e) => format!("err:{e}"),
                },
            }
        }
        "quit" => return Ok(None),
        "error" => return Err("asked to fail".into()),
        "crash" => panic!("asked to crash"),
        "spin" => {
            let mut n = 0u64;
            loop {
                n = std::hint::black_box(n.wrapping_add(1));
            }
        }
        other => format!("unknown command {other}"),
    }))
}

impl Handler for Lines {
    fn open(_ctx: &mut Ctx, info: &ConnectionInfo) -> Result<Self, String> {
        Ok(Self {
            info: info.clone(),
            pending: Vec::new(),
        })
    }

    fn data(&mut self, ctx: &mut Ctx, bytes: &[u8], out: &mut Output<'_>) -> Result<Flow, String> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw).trim_end().to_string();
            match command(ctx, &self.info, &line)? {
                Some(reply) => writeln!(out, "{reply}").map_err(|e| e.to_string())?,
                None => return Ok(Flow::Close),
            }
        }
        Ok(Flow::Continue)
    }
}

bulkhead_sdk::connection_handler!(Lines);
