//! A stateful counter, written with `paddock-sdk`: the upgrade example
//! (docs/GETTING_STARTED.md, tutorial chapter 6).
//!
//! - v1 keeps a count.
//! - v2 (`--features v2`) also keeps the last three values, and migrates v1
//!   snapshots through `#[snapshot(migrate = ...)]`.
//! - v3 (`--features v3`) is v2 refusing every snapshot: the upgrade that
//!   must revert.
//!
//! Text commands, so `paddock call counter inc` works: `inc` replies the new
//! count; `get` replies `<version> count=<n>` (and ` history=...` from v2).
//! `lifecycle.start_args = { register = "name" }` registers a name.

use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply, Snapshot};
use serde::{Deserialize, Serialize};

#[cfg(not(feature = "v2"))]
const VERSION: &str = "1.0.0";
#[cfg(all(feature = "v2", not(feature = "v3")))]
const VERSION: &str = "2.0.0";
#[cfg(feature = "v3")]
const VERSION: &str = "3.0.0";

/// v1 state.
#[cfg(not(feature = "v2"))]
#[derive(Debug, Default, Serialize, Deserialize, Snapshot)]
#[snapshot(version = 1)]
struct State {
    count: u64,
}

/// v2 state: format 2, reads format 1.
#[cfg(feature = "v2")]
#[derive(Debug, Default, Serialize, Deserialize, Snapshot)]
#[snapshot(version = 2, migrate = migrate)]
struct State {
    count: u64,
    history: Vec<u64>,
}

#[cfg(feature = "v2")]
fn migrate(from: u32, old: paddock_sdk::snapshot::Value) -> Result<State, String> {
    #[derive(Deserialize)]
    struct V1 {
        count: u64,
    }
    match from {
        1 => {
            let v1: V1 = old.deserialized().map_err(|e| e.to_string())?;
            Ok(State {
                count: v1.count,
                history: Vec::new(),
            })
        }
        other => Err(format!("unknown snapshot format {other}")),
    }
}

#[derive(Deserialize, Default)]
struct Args {
    register: Option<String>,
}

struct Counter {
    state: State,
}

impl Counter {
    fn describe(&self) -> String {
        #[cfg(feature = "v2")]
        {
            let h: Vec<String> = self.state.history.iter().map(u64::to_string).collect();
            format!(
                "{VERSION} count={} history={}",
                self.state.count,
                h.join(",")
            )
        }
        #[cfg(not(feature = "v2"))]
        {
            format!("{VERSION} count={}", self.state.count)
        }
    }

    fn inc(&mut self) -> u64 {
        self.state.count += 1;
        #[cfg(feature = "v2")]
        {
            self.state.history.push(self.state.count);
            if self.state.history.len() > 3 {
                self.state.history.remove(0);
            }
        }
        self.state.count
    }
}

impl App for Counter {
    fn init(ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let args: Args = if args.is_empty() {
            Args::default()
        } else {
            serde_json::from_slice(args).map_err(|e| format!("start_args: {e}"))?
        };
        if let Some(name) = args.register {
            ctx.register(&name)
                .map_err(|e| format!("register {name}: {e}"))?;
        }
        Ok(Self {
            state: State::default(),
        })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let reply = match msg.as_text() {
            Some("inc") => self.inc().to_string(),
            Some("get") => self.describe(),
            _ => "commands: inc, get".to_string(),
        };
        Reply::Now(Envelope::text(reply))
    }

    fn save_snapshot(&self) -> Result<Vec<u8>, String> {
        self.state.to_snapshot()
    }

    fn load_snapshot(_ctx: &mut Ctx, from_version: &str, snapshot: &[u8]) -> Result<Self, String> {
        if cfg!(feature = "v3") {
            return Err(format!("v3 refuses snapshots from {from_version}"));
        }
        Ok(Self {
            state: State::from_snapshot(snapshot)?,
        })
    }
}

paddock_sdk::app!(Counter);
