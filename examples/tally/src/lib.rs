//! A tally kept in Postgres, through `paddock:sql`: the state lives in the
//! database, not the instance or the node, so it survives the instance,
//! the node and a failover to another node that reaches the same database
//! (tutorial chapter 4).
//!
//! - `GET /tally`: `{"count": n, "recent": [{"n", "at", "by"}...], "pid"}`,
//!   the last three changes newest first.
//! - `POST /tally`: add one (or the body's number) in a transaction that
//!   also logs the change with the instance's pid, then answer as `GET`.
//! - `DELETE /tally`: back to 0, log cleared.
//!
//! `init` creates the two tables if they are missing.

use paddock_sdk::http::{Request, Response};
use paddock_sdk::sql::{self, SqlError, Value};
use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};

/// The datasource the manifest names.
const DB: &str = "main";

struct Tally;

fn schema() -> Result<(), SqlError> {
    sql::execute(
        DB,
        "CREATE TABLE IF NOT EXISTS tally (id int PRIMARY KEY, count bigint NOT NULL)",
        &[],
    )?;
    sql::execute(
        DB,
        "INSERT INTO tally VALUES (1, 0) ON CONFLICT DO NOTHING",
        &[],
    )?;
    sql::execute(
        DB,
        "CREATE TABLE IF NOT EXISTS tally_log \
         (n bigint PRIMARY KEY, at timestamptz NOT NULL DEFAULT now(), by text NOT NULL)",
        &[],
    )?;
    Ok(())
}

/// Add `by`, and log it, in one transaction.
fn add(by: i64, pid: &str) -> Result<(), SqlError> {
    let tx = sql::begin(DB)?;
    let rows = tx.query(
        "UPDATE tally SET count = count + $1 WHERE id = 1 RETURNING count",
        &[by.into()],
    )?;
    let count = rows.rows.first().and_then(|r| r[0].as_int()).unwrap_or(0);
    tx.execute(
        "INSERT INTO tally_log (n, by) VALUES ($1, $2)",
        &[count.into(), pid.into()],
    )?;
    // Dropping `tx` instead, as an early `?` above does, rolls back.
    tx.commit()
}

fn reset() -> Result<(), SqlError> {
    let tx = sql::begin(DB)?;
    tx.execute("UPDATE tally SET count = 0 WHERE id = 1", &[])?;
    tx.execute("DELETE FROM tally_log", &[])?;
    tx.commit()
}

fn read(pid: &str) -> Result<serde_json::Value, SqlError> {
    let count = sql::query(DB, "SELECT count FROM tally WHERE id = 1", &[])?
        .rows
        .first()
        .and_then(|r| r[0].as_int())
        .unwrap_or(0);
    let recent: Vec<serde_json::Value> = sql::query(
        DB,
        "SELECT n, at, by FROM tally_log ORDER BY n DESC LIMIT 3",
        &[],
    )?
    .rows
    .iter()
    .map(|r| {
        let text = |v: &Value| v.as_text().unwrap_or_default().to_string();
        serde_json::json!({ "n": r[0].as_int(), "at": text(&r[1]), "by": text(&r[2]) })
    })
    .collect();
    Ok(serde_json::json!({ "count": count, "recent": recent, "pid": pid }))
}

fn serve(pid: &str, req: &Request) -> Result<Response, SqlError> {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/tally") => {}
        ("POST", "/tally") => {
            let by = req
                .text()
                .and_then(|t| t.trim().parse::<i64>().ok())
                .unwrap_or(1);
            add(by, pid)?;
        }
        ("DELETE", "/tally") => reset()?,
        (_, "/tally") => return Ok(Response::text(405, "GET, POST or DELETE /tally")),
        _ => return Ok(Response::text(404, "not here")),
    }
    Ok(Response::json(200, &read(pid)?).unwrap_or_else(|e| Response::text(500, e.to_string())))
}

impl App for Tally {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        schema().map_err(|e| format!("creating the tables: {e}"))?;
        Ok(Self)
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let Some(req) = Request::from_envelope(&msg) else {
            return Reply::Now(Envelope::text("send HTTP requests to /tally"));
        };
        let pid = ctx.pid().to_string();
        let response = serve(&pid, &req).unwrap_or_else(|e| {
            paddock_sdk::warn!("tally: {e}");
            let status = match e {
                SqlError::Unavailable(_) | SqlError::LimitReached | SqlError::Timeout => 503,
                _ => 500,
            };
            Response::text(status, e.to_string())
        });
        Reply::Now(response.into())
    }
}

paddock_sdk::app!(Tally);
