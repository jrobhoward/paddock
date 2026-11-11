//! Test fixture: a stateful counter in three versions, for upgrade tests.
//!
//! - v1 (no features) keeps a count. Its snapshot is the count in decimal.
//! - v2 (`--features v2`) also keeps the last three values. Its snapshot is
//!   `count;h1,h2,h3`. It reads v1 and v2 snapshots.
//! - v3 (`--features v3`) is v2 whose `load-snapshot` always fails.
//!
//! `init` arguments: `register:<name>` registers the name.
//!
//! Commands: `inc` (reply the new count), `get` (reply
//! `<version> count=<n>` and, from v2 on, ` history=<h1,h2,...>`), `self`
//! (reply the pid), `trap` (panic). Also a cast `inc`, and
//! the calls `after:<ms>` (a timer that adds one when it fires),
//! `trap-link:<name>` (trap exits and link to the instance registered as
//! `<name>`; its exit adds one), `hold` (no reply until `release`) and
//! `release`.

use std::time::Duration;

use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Info, Reply};

#[cfg(not(any(feature = "v2", feature = "v3")))]
const VERSION: &str = "1.0.0";
#[cfg(all(feature = "v2", not(feature = "v3")))]
const VERSION: &str = "2.0.0";
#[cfg(feature = "v3")]
const VERSION: &str = "3.0.0";

const HISTORY: bool = cfg!(any(feature = "v2", feature = "v3"));

/// The counter app.
#[derive(Default)]
struct Counter {
    count: u64,
    history: Vec<u64>,
    /// A call answered later, by `release`.
    held: Option<CallRef>,
}

impl Counter {
    fn inc(&mut self) -> String {
        self.count += 1;
        if HISTORY {
            self.history.push(self.count);
            if self.history.len() > 3 {
                self.history.remove(0);
            }
        }
        self.count.to_string()
    }

    fn describe(&self) -> String {
        if HISTORY {
            let h: Vec<String> = self.history.iter().map(u64::to_string).collect();
            format!("{VERSION} count={} history={}", self.count, h.join(","))
        } else {
            format!("{VERSION} count={}", self.count)
        }
    }
}

impl App for Counter {
    fn init(ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let args = String::from_utf8_lossy(args).into_owned();
        for word in args.trim().trim_matches('"').split_whitespace() {
            if let Some(name) = word.strip_prefix("register:") {
                ctx.register(name)
                    .map_err(|_| format!("cannot register {name}"))?;
            }
        }
        Ok(Self::default())
    }

    fn handle_call(&mut self, ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Reply {
        let command = String::from_utf8_lossy(&msg.body).into_owned();
        let reply = match command.as_str() {
            "inc" => self.inc(),
            "hold" => {
                self.held = Some(call);
                return Reply::Later;
            }
            "release" => match self.held.take() {
                Some(held) => {
                    ctx.reply(held, Envelope::text("released"));
                    "ok".into()
                }
                None => "nothing held".into(),
            },
            other if other.starts_with("after:") => {
                let ms = other["after:".len()..].parse().unwrap_or(0);
                ctx.send_after(Duration::from_millis(ms), Envelope::text("tick"));
                "ok".into()
            }
            other if other.starts_with("trap-link:") => {
                match ctx.whereis(&other["trap-link:".len()..]) {
                    Some(pid) => {
                        ctx.trap_exits(true);
                        ctx.link(pid);
                        "ok".into()
                    }
                    None => "no such name".into(),
                }
            }
            "get" => self.describe(),
            "self" => {
                let p = ctx.pid();
                format!(
                    "{}.{}.{}@{}",
                    p.node,
                    p.id & ((1 << 40) - 1),
                    p.incarnation,
                    p.id >> 40
                )
            }
            "trap" => panic!("asked to trap"),
            other => format!("unknown command '{other}'"),
        };
        Reply::Now(Envelope::text(reply))
    }

    fn handle_cast(&mut self, _ctx: &mut Ctx, msg: Envelope) {
        if msg.body == b"inc" {
            self.inc();
        }
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
        if matches!(info, Info::Timer { .. } | Info::Exit { .. }) {
            self.inc();
        }
    }

    fn save_snapshot(&self) -> Result<Vec<u8>, String> {
        Ok(if HISTORY {
            let h: Vec<String> = self.history.iter().map(u64::to_string).collect();
            format!("{};{}", self.count, h.join(","))
        } else {
            self.count.to_string()
        }
        .into_bytes())
    }

    fn load_snapshot(_ctx: &mut Ctx, from_version: &str, snapshot: &[u8]) -> Result<Self, String> {
        if cfg!(feature = "v3") {
            return Err(format!("v3 cannot read snapshots from {from_version}"));
        }
        let text = String::from_utf8(snapshot.to_vec()).map_err(|e| e.to_string())?;
        let (count, history) = text.split_once(';').unwrap_or((&text, ""));
        let count: u64 = count
            .parse()
            .map_err(|_| format!("bad count in '{text}'"))?;
        let history = if HISTORY {
            history.split(',').filter_map(|h| h.parse().ok()).collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            count,
            history,
            ..Self::default()
        })
    }
}

bulkhead_sdk::app!(Counter);
