//! The counter fixture as in-process test apps (ROADMAP Z9d), in its three
//! versions, for upgrade tests: `bulkhead-fixture-counter`'s
//! crate docs list its commands, and its replies are kept.
//!
//! - [`V1`] keeps a count; its snapshot is the count in decimal.
//! - [`V2`] also keeps the last three values; its snapshot is
//!   `count;h1,h2,h3`, and it reads v1's and v2's.
//! - [`V3`] is v2 whose `load_snapshot` always fails.

use std::marker::PhantomData;

use super::chaos::show_pid;
use super::{Envelope, ExitReason, Info, TestApp, TestCtx};

/// A counter version: its name and what it keeps.
pub trait Version: Send + Sync + 'static {
    /// `1.0.0`, `2.0.0` or `3.0.0`.
    const NAME: &'static str;
    /// Whether it keeps the last three values.
    const HISTORY: bool;
    /// Whether it refuses every snapshot.
    const REFUSES: bool;
}

/// Version 1.
#[derive(Debug, Default)]
pub struct V1;
/// Version 2.
#[derive(Debug, Default)]
pub struct V2;
/// Version 3.
#[derive(Debug, Default)]
pub struct V3;

impl Version for V1 {
    const NAME: &'static str = "1.0.0";
    const HISTORY: bool = false;
    const REFUSES: bool = false;
}

impl Version for V2 {
    const NAME: &'static str = "2.0.0";
    const HISTORY: bool = true;
    const REFUSES: bool = false;
}

impl Version for V3 {
    const NAME: &'static str = "3.0.0";
    const HISTORY: bool = true;
    const REFUSES: bool = true;
}

/// The counter at version `V`.
#[derive(Debug)]
pub struct Counter<V: Version> {
    count: u64,
    history: Vec<u64>,
    /// A call answered later, by `release`.
    held: Option<u64>,
    version: PhantomData<V>,
}

impl<V: Version> Default for Counter<V> {
    fn default() -> Self {
        Self {
            count: 0,
            history: Vec::new(),
            held: None,
            version: PhantomData,
        }
    }
}

fn text(body: String) -> Envelope {
    Envelope::new("text/plain", body.into_bytes())
}

impl<V: Version> Counter<V> {
    fn inc(&mut self) -> String {
        self.count += 1;
        if V::HISTORY {
            self.history.push(self.count);
            if self.history.len() > 3 {
                self.history.remove(0);
            }
        }
        self.count.to_string()
    }

    fn describe(&self) -> String {
        if V::HISTORY {
            let h: Vec<String> = self.history.iter().map(u64::to_string).collect();
            format!("{} count={} history={}", V::NAME, self.count, h.join(","))
        } else {
            format!("{} count={}", V::NAME, self.count)
        }
    }
}

impl<V: Version> TestApp for Counter<V> {
    async fn init(cx: &mut TestCtx, args: Vec<u8>) -> Result<Self, String> {
        let args = String::from_utf8_lossy(&args).into_owned();
        for word in args.trim().trim_matches('"').split_whitespace() {
            if let Some(name) = word.strip_prefix("register:") {
                cx.register(name)
                    .await
                    .map_err(|_| format!("cannot register {name}"))?;
            }
        }
        Ok(Self::default())
    }

    async fn load_snapshot(
        _cx: &mut TestCtx,
        from_version: String,
        snapshot: Vec<u8>,
    ) -> Result<Self, String> {
        if V::REFUSES {
            return Err(format!("v3 cannot read snapshots from {from_version}"));
        }
        let text = String::from_utf8(snapshot).map_err(|e| e.to_string())?;
        let (count, history) = text.split_once(';').unwrap_or((&text, ""));
        let count: u64 = count
            .parse()
            .map_err(|_| format!("bad count in '{text}'"))?;
        let history = if V::HISTORY {
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

    async fn handle_call(
        &mut self,
        cx: &mut TestCtx,
        call_ref: u64,
        msg: Envelope,
    ) -> Result<Option<Envelope>, String> {
        let command = String::from_utf8_lossy(&msg.body).into_owned();
        let reply = match command.as_str() {
            "inc" => self.inc(),
            "hold" => {
                self.held = Some(call_ref);
                return Ok(None);
            }
            "release" => match self.held.take() {
                Some(held) => {
                    cx.reply(held, text("released".into())).await;
                    "ok".into()
                }
                None => "nothing held".into(),
            },
            other if other.starts_with("after:") => {
                let ms = other["after:".len()..].parse().unwrap_or(0);
                cx.send_after(ms, text("tick".into())).await;
                "ok".into()
            }
            other if other.starts_with("trap-link:") => {
                match cx.whereis(&other["trap-link:".len()..]).await {
                    Some(pid) => {
                        cx.trap_exits(true).await;
                        cx.link(pid).await;
                        "ok".into()
                    }
                    None => "no such name".into(),
                }
            }
            "get" => self.describe(),
            "self" => {
                let me = cx.self_pid().await;
                show_pid(me)
            }
            "trap" => return Err("asked to trap".into()),
            other => format!("unknown command '{other}'"),
        };
        Ok(Some(text(reply)))
    }

    async fn handle_cast(&mut self, _cx: &mut TestCtx, msg: Envelope) -> Result<(), String> {
        if msg.body == b"inc" {
            self.inc();
        }
        Ok(())
    }

    async fn handle_info(&mut self, _cx: &mut TestCtx, info: Info) -> Result<(), String> {
        if matches!(info, Info::Timer { .. } | Info::Exit { .. }) {
            self.inc();
        }
        Ok(())
    }

    async fn terminate(&mut self, _cx: &mut TestCtx, _reason: ExitReason) {}

    fn save_snapshot(&mut self) -> Result<Vec<u8>, String> {
        Ok(if V::HISTORY {
            let h: Vec<String> = self.history.iter().map(u64::to_string).collect();
            format!("{};{}", self.count, h.join(","))
        } else {
            self.count.to_string()
        }
        .into_bytes())
    }
}
