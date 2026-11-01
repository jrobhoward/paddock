//! Test fixture: calls and casts timed inside an app, for `bulkhead-host`'s
//! `cluster_throughput` benchmark. One source, built as a component and as
//! a native program (feature `native`), so both kinds run the same code.
//!
//! Text calls:
//! - `echo:<text>`: `<text>`.
//! - `mirror:...`: the same envelope back.
//! - `count`: how many `count` messages (calls and casts) the instance has
//!   had, this one included.
//! - `calls:<pid>:<n>`: `<n>` calls of `echo:x` to `<pid>`
//!   (`node.id.incarnation`), one at a time; replies `<total> <p50> <p99>`
//!   in nanoseconds, from the app's own clock.
//! - `casts:<pid>:<n>`: `<n>` casts of `count` to `<pid>`, then a call of
//!   `echo:x`, which `<pid>` handles after them (messages between two
//!   instances keep their order), retried while refused as overloaded;
//!   replies the nanoseconds until that call returned. Casts refused when
//!   sent are dropped, and show in `<pid>`'s count.
//!
//! A cast of `count` counts as a call does.

use std::time::{Duration, Instant};

use bulkhead_sdk::{App, CallError, CallRef, Ctx, Envelope, Pid, Reply};

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct MsgBench {
    count: u64,
}

fn pid(text: &str) -> Result<Pid, String> {
    let mut parts = text.split('.');
    let mut next = || parts.next().ok_or_else(|| format!("not a pid: {text}"));
    let node = next()?.parse().map_err(|_| format!("not a pid: {text}"))?;
    let id = next()?.parse().map_err(|_| format!("not a pid: {text}"))?;
    let incarnation = next()?.parse().map_err(|_| format!("not a pid: {text}"))?;
    Ok(Pid {
        node,
        id,
        incarnation,
    })
}

fn number(text: Option<&str>) -> u64 {
    text.and_then(|t| t.parse().ok()).unwrap_or(0)
}

/// `n` calls of `echo:x` to `to`, one at a time: total, p50 and p99 ns.
fn calls(ctx: &mut Ctx, to: Pid, n: u64) -> Result<String, String> {
    let mut ns = Vec::with_capacity(usize::try_from(n).unwrap_or(0));
    let started = Instant::now();
    for _ in 0..n {
        let t = Instant::now();
        ctx.call(to, Envelope::text("echo:x"), TIMEOUT)
            .map_err(|e| format!("{e:?}"))?;
        ns.push(t.elapsed().as_nanos());
    }
    let total = started.elapsed().as_nanos();
    ns.sort_unstable();
    let at = |p: f64| {
        let i = ((ns.len().saturating_sub(1)) as f64 * p) as usize;
        ns.get(i).copied().unwrap_or(0)
    };
    Ok(format!("{total} {} {}", at(0.5), at(0.99)))
}

/// `n` casts of `count` to `to`, then a call behind them: the ns until
/// it returned.
fn casts(ctx: &mut Ctx, to: Pid, n: u64) -> Result<String, String> {
    let started = Instant::now();
    for _ in 0..n {
        ctx.cast(to, Envelope::text("count"));
    }
    // Refused while the casts still fill the queue to `to`'s node.
    loop {
        match ctx.call(to, Envelope::text("echo:x"), TIMEOUT) {
            Err(CallError::Overloaded) => {}
            Err(e) => return Err(format!("{e:?}")),
            Ok(_) => break,
        }
    }
    Ok(started.elapsed().as_nanos().to_string())
}

impl App for MsgBench {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self::default())
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let (verb, rest) = text.split_once(':').unwrap_or((text, ""));
        let mut f = rest.split(':');
        let reply = match verb {
            "echo" => rest.to_string(),
            "mirror" => return Reply::Now(msg),
            "count" => {
                self.count += 1;
                self.count.to_string()
            }
            "calls" => match pid(f.next().unwrap_or("")) {
                Ok(to) => calls(ctx, to, number(f.next())).unwrap_or_else(|e| format!("err:{e}")),
                Err(e) => format!("err:{e}"),
            },
            "casts" => match pid(f.next().unwrap_or("")) {
                Ok(to) => casts(ctx, to, number(f.next())).unwrap_or_else(|e| format!("err:{e}")),
                Err(e) => format!("err:{e}"),
            },
            _ => format!("err:unknown command {verb}"),
        };
        Reply::Now(Envelope::text(reply))
    }

    fn handle_cast(&mut self, _ctx: &mut Ctx, msg: Envelope) {
        if msg.as_text() == Some("count") {
            self.count += 1;
        }
    }
}

bulkhead_sdk::app!(MsgBench);
