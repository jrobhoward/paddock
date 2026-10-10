//! The operator's side: a line protocol on the `hmi` listener, one
//! instance per connection. Each line is forwarded to the `points` app's
//! `points-api` instance and its reply written back; `quit` closes the
//! connection. See `examples/scada/points` for the commands.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use paddock_sdk::connection::{Connection, ConnectionInfo, Stream};
use paddock_sdk::{Ctx, Envelope};

struct Hmi;

fn ask(ctx: &mut Ctx, line: &str) -> String {
    let Some(pid) = ctx.whereis("points-api") else {
        return "err:points is not running".to_string();
    };
    match ctx.call(pid, Envelope::text(line), Duration::from_secs(5)) {
        Ok(reply) => reply.as_text().unwrap_or("").to_string(),
        Err(e) => format!("err:{e}"),
    }
}

impl Connection for Hmi {
    fn serve(ctx: &mut Ctx, _info: ConnectionInfo, stream: Stream) -> Result<(), String> {
        let mut out = stream.clone_writer();
        for line in BufReader::new(stream).lines() {
            let line = line.map_err(|e| e.to_string())?;
            if line.trim() == "quit" {
                return Ok(());
            }
            let reply = ask(ctx, line.trim());
            writeln!(out, "{reply}").map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

paddock_sdk::connection!(Hmi);
