//! Spike S31's native app, on `bulkhead_native::app` and run through
//! `bulkhead-runner`: requests to the node timed from inside a callback.
//!
//! `self:<n>` asks for its own pid `n` times, `whereis:<n>` looks up a
//! name `n` times; each replies the nanoseconds the loop took. `echo:<x>`
//! replies `<x>`.

use std::process::ExitCode;
use std::time::Instant;

use bulkhead_native::app::{App, CallRef, Ctx, serve};
use bulkhead_native::protocol::Envelope;

struct S31;

impl App for S31 {
    fn init(_ctx: &mut Ctx, _args: Vec<u8>) -> Result<Self, String> {
        Ok(Self)
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Option<Envelope> {
        let text = String::from_utf8(msg.body).unwrap_or_default();
        let (verb, arg) = text.split_once(':').unwrap_or((&text, ""));
        let n: u64 = arg.parse().unwrap_or(0);
        let reply = match verb {
            "self" => {
                let t = Instant::now();
                for _ in 0..n {
                    std::hint::black_box(ctx.self_pid());
                }
                t.elapsed().as_nanos().to_string()
            }
            "whereis" => {
                let t = Instant::now();
                for _ in 0..n {
                    std::hint::black_box(ctx.whereis("s31-nobody"));
                }
                t.elapsed().as_nanos().to_string()
            }
            "echo" => arg.to_string(),
            _ => "err:unknown".into(),
        };
        Some(Envelope {
            content_type: "text/plain".into(),
            schema: None,
            body: reply.into_bytes(),
        })
    }
}

fn main() -> ExitCode {
    serve::<S31>()
}
