//! Two apps from one component, chosen by `start_args`: `pong` answers typed
//! CBOR `Ping` calls; `ping` finds `pong` by its registered name, calls it,
//! and monitors it.
//!
//! - `pong` (`start_args = { role = "pong" }`): registers `pong`; replies
//!   `Pong { n: n + 1 }` to `Ping { n }`.
//! - `ping` (`start_args = { role = "ping" }`), text commands:
//!   - `ping:<n>`: calls `pong` with `Ping { n }`, replies `pong <n+1>` or
//!     the error.
//!   - `downs`: how many `down` notifications it has seen for `pong`.

use std::time::Duration;

use paddock_sdk::{App, CallRef, Ctx, Envelope, Info, MonitorRef, Reply};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Ping {
    n: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Pong {
    n: u64,
}

#[derive(Deserialize)]
struct Args {
    role: String,
}

enum PingPong {
    Pong,
    Ping {
        monitor: Option<MonitorRef>,
        downs: u64,
    },
}

impl PingPong {
    fn watch_pong(ctx: &mut Ctx) -> Option<MonitorRef> {
        ctx.whereis("pong").map(|pid| ctx.monitor(pid))
    }

    fn ping(ctx: &mut Ctx, arg: &str) -> String {
        let Ok(n) = arg.parse::<u64>() else {
            return format!("not a number: {arg}");
        };
        let Some(pong) = ctx.whereis("pong") else {
            return "pong is not running".to_string();
        };
        let request = match Envelope::cbor(&Ping { n }) {
            Ok(env) => env.with_schema("ping-pong.ping.v1"),
            Err(e) => return e.to_string(),
        };
        match ctx.call(pong, request, Duration::from_secs(1)) {
            Ok(reply) => match reply.decode::<Pong>() {
                Ok(p) => format!("pong {}", p.n),
                Err(e) => format!("bad reply: {e}"),
            },
            Err(e) => format!("call failed: {e}"),
        }
    }
}

impl App for PingPong {
    fn init(ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let args: Args = serde_json::from_slice(args).map_err(|e| format!("start_args: {e}"))?;
        match args.role.as_str() {
            "pong" => {
                ctx.register("pong")
                    .map_err(|e| format!("register pong: {e}"))?;
                Ok(Self::Pong)
            }
            "ping" => Ok(Self::Ping {
                monitor: Self::watch_pong(ctx),
                downs: 0,
            }),
            other => Err(format!("unknown role {other}")),
        }
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        match self {
            Self::Pong => match msg.decode::<Ping>() {
                Ok(ping) => match Envelope::cbor(&Pong { n: ping.n + 1 }) {
                    Ok(env) => Reply::Now(env.with_schema("ping-pong.pong.v1")),
                    Err(e) => Reply::Now(Envelope::text(e.to_string())),
                },
                Err(e) => Reply::Now(Envelope::text(format!("expected a Ping: {e}"))),
            },
            Self::Ping { downs, monitor } => {
                let text = msg.as_text().unwrap_or("");
                let reply = match text.split_once(':') {
                    Some(("ping", n)) => {
                        if monitor.is_none() {
                            *monitor = Self::watch_pong(ctx);
                        }
                        Self::ping(ctx, n)
                    }
                    _ if text == "downs" => downs.to_string(),
                    _ => "commands: ping:<n>, downs".to_string(),
                };
                Reply::Now(Envelope::text(reply))
            }
        }
    }

    fn handle_info(&mut self, ctx: &mut Ctx, info: Info) {
        if let (Self::Ping { downs, monitor }, Info::Down { .. }) = (self, info) {
            *downs += 1;
            // Watch the restarted pong (if it is back yet).
            *monitor = Self::watch_pong(ctx);
        }
    }
}

paddock_sdk::app!(PingPong);
