//! Entries in the node's shared `ledger` bucket, written with `paddock-sdk`:
//! the example for replicated (`cluster`) buckets, and the app the
//! replication soak writes through.
//!
//! Text commands, so `paddock call ledger "put a 1"` works:
//! - `put <key> <value>`: `ok`, or `err:<error>`. On a replicated bucket it
//!   returns once the pair's primary has committed the write; an error that
//!   says it may yet commit means the outcome is unknown.
//! - `get <key>`: the value from this node's copy, or `none`.
//! - `latest <key>`: the value read through the primary (linearizable), or
//!   `none`.
//! - `keys`: every key, joined by `,`.

use paddock_sdk::kv::Bucket;
use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply, watch};

const LEDGER: &str = "ledger";

struct Ledger;

fn command(text: &str) -> Result<String, String> {
    let bucket = Bucket::open(LEDGER).map_err(|e| e.to_string())?;
    let mut words = text.splitn(3, ' ');
    let verb = words.next().unwrap_or("");
    let key = words.next().unwrap_or("");
    let value = words.next().unwrap_or("");
    let shown = |v: Option<Vec<u8>>| {
        v.map_or_else(
            || "none".into(),
            |v| String::from_utf8_lossy(&v).into_owned(),
        )
    };
    match verb {
        "put" if !key.is_empty() => {
            bucket
                .set(key, value.as_bytes())
                .map_err(|e| e.to_string())?;
            Ok("ok".into())
        }
        "get" if !key.is_empty() => Ok(shown(bucket.get(key).map_err(|e| e.to_string())?)),
        "latest" if !key.is_empty() => Ok(shown(
            watch::get_linearizable(LEDGER, key)
                .map_err(|e| e.to_string())?
                .map(|e| e.value),
        )),
        "keys" => Ok(bucket.keys().map_err(|e| e.to_string())?.join(",")),
        _ => Err("commands: put <key> <value>, get <key>, latest <key>, keys".into()),
    }
}

impl App for Ledger {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self)
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let reply = command(text).unwrap_or_else(|e| format!("err:{e}"));
        Reply::Now(Envelope::text(reply))
    }
}

paddock_sdk::app!(Ledger);
