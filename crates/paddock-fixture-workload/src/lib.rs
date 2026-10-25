//! Benchmark fixture: `paddock-workload`'s workloads as an app, for
//! `paddock-host`'s `app_cost` (ROADMAP B11). The inputs are generated in
//! `init`, outside the timed calls.
//!
//! Text commands; errors reply `err:<error>`:
//! - `run:<workload>:<n>`: run it `n` times; the checksum.
//! - `sizes`: the input sizes in bytes, `orders,log,markdown`.

use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};
use paddock_workload::Inputs;

struct Workload {
    inputs: Inputs,
}

impl Workload {
    fn command(&self, text: &str) -> Result<String, String> {
        let mut parts = text.split(':');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("run"), Some(name), Some(n)) => {
                let n: u32 = n.parse().map_err(|_| format!("bad count {n}"))?;
                paddock_workload::run(&self.inputs, name, n)
                    .map(|sum| sum.to_string())
                    .ok_or_else(|| format!("unknown workload {name}"))
            }
            (Some("sizes"), None, None) => {
                let (orders, log, markdown) = self.inputs.sizes();
                Ok(format!("{orders},{log},{markdown}"))
            }
            _ => Err("commands: run:<workload>:<n>, sizes".into()),
        }
    }
}

impl App for Workload {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self {
            inputs: Inputs::new(),
        })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let reply = self.command(text).unwrap_or_else(|e| format!("err:{e}"));
        Reply::Now(Envelope::text(reply))
    }
}

paddock_sdk::app!(Workload);
