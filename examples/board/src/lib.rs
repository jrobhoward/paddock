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

use paddock_sdk::http::{Request, Response};
use paddock_sdk::kv::Bucket;
use paddock_sdk::watch::{self, Change};
use paddock_sdk::{App, CallRef, Ctx, Envelope, Info, Reply};
use serde::Deserialize;

/// The shared bucket, and the key the count is kept under.
const BOARD: &str = "board";
const KEY: &str = "count";

#[derive(Debug, Default, Deserialize)]
struct Args {
    #[serde(default)]
    watch: bool,
}

enum Board {
    Web,
    Watch { seen: u64 },
}

fn read(board: &Bucket) -> Result<i64, String> {
    Ok(board
        .get_value::<i64>(KEY)
        .map_err(|e| e.to_string())?
        .unwrap_or(0))
}

fn serve(ctx: &Ctx, req: &Request) -> Result<Response, String> {
    let pid = ctx.pid().to_string();
    let board = Bucket::open(BOARD).map_err(|e| e.to_string())?;
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
    let body = serde_json::json!({ "count": count, "pid": pid });
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
            return Ok(Self::Web);
        }
        let started = watch::watch(BOARD, KEY).map_err(|e| e.to_string())?;
        paddock_sdk::info!("watching {BOARD}/{KEY} from revision {}", started.revision);
        Ok(Self::Watch { seen: 0 })
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let reply = match (&*self, Request::from_envelope(&msg)) {
            (Self::Web, Some(req)) => serve(ctx, &req)
                .unwrap_or_else(|e| Response::text(500, e))
                .into(),
            (Self::Watch { seen }, _) => Envelope::text(seen.to_string()),
            (Self::Web, None) => Envelope::text("send HTTP requests to /board"),
        };
        Reply::Now(reply)
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
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
