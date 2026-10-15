//! A counter kept on a bulletin board: the node's shared `board` bucket,
//! on disk. The instance holds no state of its own, so when it crashes and
//! its supervisor restarts it, the count is still there. One component,
//! two apps (each its own manifest):
//!
//! - `board` (`paddock.toml`) serves HTTP through the node's ingress. Every
//!   response carries `x-paddock-pid`, the instance that answered.
//!   - `GET /board`: `{"count": n, "pid": "<...>"}`.
//!   - `POST /board`: add one (or the body's number) and return the same.
//!   - `DELETE /board`: reset to 0.
//!   - `POST /board/crash`: trap, as a bug would. The request fails; the
//!     supervisor restarts the instance with a new incarnation.
//! - `board-watch` (`watch.toml`, start args `{"watch": true}`) watches the
//!   counter on the board and logs every change (`paddock logs
//!   board-watch`); a call with `seen` replies how many it has seen.
//! - `board` as a hot standby (`hot-standby.toml`, start args
//!   `{"replicate": true}`, tutorial chapter 3): runs on both nodes of an HA
//!   pair and learns its role from the `failover-role` host event. Every
//!   instance joins the `board` process group, across the pair. The primary
//!   writes the new count to its own board, then calls every other member
//!   with `replicate:<count>` and answers the client only after, so a write
//!   the client saw is on the standby's board too (`"replicas"` says on how
//!   many). The standby refuses writes (503) and serves reads from its own
//!   board, which the primary keeps current.

use std::time::Duration;

use paddock_sdk::http::{Request, Response};
use paddock_sdk::kv::Bucket;
use paddock_sdk::watch::{self, Change};
use paddock_sdk::{App, CallRef, Ctx, Envelope, Info, Reply, groups};
use serde::Deserialize;

/// The shared bucket, and the key the count is kept under.
const BOARD: &str = "board";
const KEY: &str = "count";
/// The process group the board's instances join when they replicate.
const GROUP: &str = "board";
/// How long the primary waits for each replica.
const REPLICA_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Default, Deserialize)]
struct Args {
    #[serde(default)]
    watch: bool,
    #[serde(default)]
    replicate: bool,
}

/// This node's part in its HA pair, from the `failover-role` host event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Not told yet, or not replicating.
    Alone,
    Primary,
    Standby,
}

enum Board {
    Web { replicate: bool, role: Role },
    Watch { seen: u64 },
}

fn read(board: &Bucket) -> Result<i64, String> {
    Ok(board
        .get_value::<i64>(KEY)
        .map_err(|e| e.to_string())?
        .unwrap_or(0))
}

/// Write `count` to every other member of the group; how many took it.
fn replicate(ctx: &mut Ctx, count: i64) -> usize {
    let me = ctx.pid();
    let others: Vec<_> = groups::members(GROUP)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| *p != me)
        .collect();
    let mut took = 0;
    for other in others {
        let msg = Envelope::text(format!("replicate:{count}"));
        match ctx.call(other, msg, REPLICA_TIMEOUT) {
            Ok(_) => took += 1,
            Err(e) => paddock_sdk::warn!("replica {other} missed count {count}: {e}"),
        }
    }
    took
}

fn serve(ctx: &mut Ctx, req: &Request, replicate_to: Option<Role>) -> Result<Response, String> {
    let pid = ctx.pid().to_string();
    let board = Bucket::open(BOARD).map_err(|e| e.to_string())?;
    let write = req.method == "POST" || req.method == "DELETE";
    if write && replicate_to == Some(Role::Standby) {
        return Ok(Response::text(503, "standby: writes go to the primary"));
    }
    let count = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/board") => read(&board)?,
        ("POST", "/board") => {
            let by = req
                .text()
                .and_then(|t| t.trim().parse::<i64>().ok())
                .unwrap_or(1);
            let count = read(&board)?.saturating_add(by);
            board.set_value(KEY, &count).map_err(|e| e.to_string())?;
            count
        }
        ("DELETE", "/board") => {
            board.set_value(KEY, &0i64).map_err(|e| e.to_string())?;
            0
        }
        ("POST", "/board/crash") => {
            paddock_sdk::warn!("crashing on request");
            // A trap, as a bug would raise; the supervisor restarts us.
            std::process::abort();
        }
        (_, "/board" | "/board/crash") => {
            return Ok(Response::text(
                405,
                "GET, POST or DELETE /board; POST /board/crash",
            ));
        }
        _ => return Ok(Response::text(404, "not here")),
    };
    let mut body = serde_json::json!({ "count": count, "pid": pid });
    if write && replicate_to.is_some() {
        body["replicas"] = replicate(ctx, count).into();
    }
    Response::json(200, &body)
        .map(|r| r.header("x-paddock-pid", pid))
        .map_err(|e| e.to_string())
}

impl App for Board {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let args: Args = if args.is_empty() {
            Args::default()
        } else {
            serde_json::from_slice(args).map_err(|e| format!("start_args: {e}"))?
        };
        if !args.watch {
            if args.replicate {
                groups::join(GROUP).map_err(|e| e.to_string())?;
            }
            return Ok(Self::Web {
                replicate: args.replicate,
                role: Role::Alone,
            });
        }
        let started = watch::watch(BOARD, KEY).map_err(|e| e.to_string())?;
        paddock_sdk::info!("watching {BOARD}/{KEY} from revision {}", started.revision);
        Ok(Self::Watch { seen: 0 })
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let reply = match (&*self, Request::from_envelope(&msg)) {
            (Self::Web { replicate, role }, Some(req)) => {
                let replicate_to = replicate.then_some(*role);
                serve(ctx, &req, replicate_to)
                    .unwrap_or_else(|e| Response::text(500, e))
                    .into()
            }
            (Self::Watch { seen }, _) => Envelope::text(seen.to_string()),
            (Self::Web { .. }, None) => match msg
                .as_text()
                .and_then(|t| t.strip_prefix("replicate:"))
            {
                // The primary's write, for this node's board.
                Some(n) => match n.parse::<i64>() {
                    Ok(count) => match Bucket::open(BOARD).and_then(|b| b.set_value(KEY, &count)) {
                        Ok(()) => Envelope::text("ok"),
                        Err(e) => Envelope::text(format!("error: {e}")),
                    },
                    Err(e) => Envelope::text(format!("error: {e}")),
                },
                None => Envelope::text("send HTTP requests to /board"),
            },
        };
        Reply::Now(reply)
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
        if let (Self::Web { role, .. }, Info::Event { name, msg }) = (&mut *self, &info)
            && name == "failover-role"
        {
            *role = match msg.as_text() {
                Some("primary") => Role::Primary,
                Some("standby") => Role::Standby,
                _ => Role::Alone,
            };
            paddock_sdk::info!("role: {role:?}");
            return;
        }
        let (Self::Watch { seen }, Some(change)) = (self, Change::from_info(&info)) else {
            return;
        };
        *seen += 1;
        let now = Bucket::open(&change.bucket)
            .and_then(|b| b.get_value::<i64>(&change.key))
            .ok()
            .flatten();
        match now {
            Some(count) => {
                paddock_sdk::info!("board: count is {count} (revision {})", change.revision)
            }
            None => paddock_sdk::info!("board: count removed (revision {})", change.revision),
        }
    }
}

paddock_sdk::app!(Board);
