//! The operator's view of the point table, over HTTP through the node's
//! ingress: a pool of instances on every node, each reading the shared
//! `points` bucket that the `points` app keeps. Every response carries
//! `x-paddock-pid`, the instance that answered.
//!
//! - `GET /scada/points`: `{"revision": r, "pid": "<...>", "points":
//!   {"pump-1/holding/0": 17, ...}, "devices": {"pump-1": "online"}}`.
//! - `GET /scada/points?after=R&wait_ms=N`: the same, but held until the
//!   bucket's revision is no longer `R`, or `N` ms have passed (at most
//!   25 s, under the route's 30 s timeout). A client long-polls by passing
//!   the revision of the last answer it got. Any other revision answers at
//!   once, so a client that moves to the other node of a pair, whose
//!   bucket has its own revisions, is never held on a stale one.
//!
//! Alarms, acknowledgements, device writes and the event log are the
//! `control` app's routes.

use std::collections::BTreeMap;
use std::time::Duration;

use paddock_sdk::http::{Request, Response};
use paddock_sdk::kv::Bucket;
use paddock_sdk::watch::{self, Change};
use paddock_sdk::{App, CallRef, Ctx, Envelope, Info, Reply, TimerRef};
use serde::Serialize;

/// The shared bucket the `points` app writes.
const POINTS: &str = "points";
/// The longest a long poll is held.
const MAX_WAIT: Duration = Duration::from_secs(25);
/// A timer's message: the waiter it ends.
const TIMER: &str = "wait:";

/// A long poll waiting for the next change.
struct Waiter {
    id: u64,
    call: CallRef,
    /// Cancelled when a change answers first, so timers do not pile up
    /// toward `timers_max` while the table changes often.
    timer: TimerRef,
}

#[derive(Default)]
struct Hmi {
    waiting: Vec<Waiter>,
    next: u64,
}

#[derive(Serialize)]
struct Table {
    revision: u64,
    pid: String,
    points: BTreeMap<String, u16>,
    devices: BTreeMap<String, String>,
}

fn table(ctx: &Ctx) -> Result<Table, String> {
    // The revision first: a write after it is in the table or comes next.
    let revision = watch::revision(POINTS).map_err(|e| e.to_string())?;
    let bucket = Bucket::open(POINTS).map_err(|e| e.to_string())?;
    let keys = bucket.keys().map_err(|e| e.to_string())?;
    let values = bucket.get_many(&keys).map_err(|e| e.to_string())?;
    let mut t = Table {
        revision,
        pid: ctx.pid().to_string(),
        points: BTreeMap::new(),
        devices: BTreeMap::new(),
    };
    for (key, value) in keys.into_iter().zip(values) {
        let Some(value) = value else { continue };
        let value = String::from_utf8_lossy(&value).into_owned();
        match key.strip_suffix("/status") {
            Some(device) => {
                t.devices.insert(device.to_string(), value);
            }
            None => {
                if let Ok(v) = value.parse() {
                    t.points.insert(key, v);
                }
            }
        }
    }
    Ok(t)
}

fn answer(ctx: &Ctx) -> Envelope {
    let pid = ctx.pid().to_string();
    table(ctx)
        .and_then(|t| Response::json(200, &t).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| Response::text(500, e))
        .header("x-paddock-pid", pid)
        .into()
}

impl Hmi {
    fn get(&mut self, ctx: &mut Ctx, call: CallRef, req: &Request) -> Reply {
        let after = req.param("after").and_then(|a| a.parse::<u64>().ok());
        let now = watch::revision(POINTS).ok();
        if after.is_none() || after != now {
            return Reply::Now(answer(ctx));
        }
        let wait = req
            .param("wait_ms")
            .and_then(|w| w.parse::<u64>().ok())
            .map_or(MAX_WAIT, Duration::from_millis)
            .min(MAX_WAIT);
        let id = self.next;
        self.next += 1;
        let timer = ctx.send_after(wait, Envelope::text(format!("{TIMER}{id}")));
        self.waiting.push(Waiter { id, call, timer });
        Reply::Later
    }
}

impl App for Hmi {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        // Every point and status: any write to the bucket ends the waits.
        watch::watch_prefix(POINTS, "").map_err(|e| format!("watch {POINTS}: {e}"))?;
        Ok(Self::default())
    }

    fn handle_call(&mut self, ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Reply {
        let Some(req) = Request::from_envelope(&msg) else {
            return Reply::Now(Envelope::text("send HTTP requests to /scada/points"));
        };
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/scada/points") => self.get(ctx, call, &req),
            (_, "/scada/points") => Reply::Now(Response::text(405, "GET /scada/points").into()),
            _ => Reply::Now(Response::text(404, "not here").into()),
        }
    }

    fn handle_info(&mut self, ctx: &mut Ctx, info: Info) {
        if Change::from_info(&info).is_some() {
            if self.waiting.is_empty() {
                return;
            }
            let reply = answer(ctx);
            for w in std::mem::take(&mut self.waiting) {
                ctx.cancel_timer(w.timer);
                ctx.reply(w.call, reply.clone());
            }
            return;
        }
        let Info::Timer { msg, .. } = info else {
            return;
        };
        let Some(id) = msg
            .as_text()
            .and_then(|t| t.strip_prefix(TIMER))
            .and_then(|id| id.parse::<u64>().ok())
        else {
            return;
        };
        // Answered already when a change came first.
        if let Some(at) = self.waiting.iter().position(|w| w.id == id) {
            let w = self.waiting.swap_remove(at);
            let reply = answer(ctx);
            ctx.reply(w.call, reply);
        }
    }
}

paddock_sdk::app!(Hmi);
