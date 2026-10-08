//! An app that fails on purpose, written with `paddock-sdk`, for the Phase
//! 1 failure tests (crash loop, runaway code, memory cap).
//!
//! `start_args = { fail = "error" | "trap" | "spin" }` makes `init` fail.
//! Text commands: `echo:<text>`, `trap` (panic), `spin` (loop forever),
//! `alloc:<mib>` (allocate and keep `<mib>` MiB), `log:<n>` (write `<n>`
//! debug records, for measuring what a filtered record costs).

use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Args {
    fail: Option<String>,
}

#[derive(Default)]
struct Chaos {
    hoard: Vec<Vec<u8>>,
}

fn spin() -> ! {
    let mut n = 0u64;
    loop {
        n = std::hint::black_box(n.wrapping_add(1));
    }
}

impl App for Chaos {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let args: Args = if args.is_empty() {
            Args::default()
        } else {
            serde_json::from_slice(args).map_err(|e| format!("start_args: {e}"))?
        };
        match args.fail.as_deref() {
            None => Ok(Self::default()),
            Some("error") => Err("asked to fail".to_string()),
            Some("trap") => panic!("asked to trap in init"),
            Some("spin") => spin(),
            Some(other) => Err(format!("unknown failure {other}")),
        }
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let (verb, arg) = text.split_once(':').unwrap_or((text, ""));
        let reply = match verb {
            "echo" => arg.to_string(),
            "trap" => panic!("asked to trap"),
            "spin" => spin(),
            "alloc" => {
                let mib: usize = arg.parse().unwrap_or(0);
                self.hoard.push(std::hint::black_box(vec![1u8; mib << 20]));
                "ok".to_string()
            }
            "log" => {
                let n: u32 = arg.parse().unwrap_or(0);
                for i in 0..n {
                    paddock_sdk::debug!("record {i} of {n}");
                }
                "ok".to_string()
            }
            _ => "commands: echo:<text>, trap, spin, alloc:<mib>, log:<n>".to_string(),
        };
        Reply::Now(Envelope::text(reply))
    }
}

paddock_sdk::app!(Chaos);
