//! Test fixture: an app that uses `wasi:config` and `wasi:keyvalue` through
//! `paddock-sdk`'s wrappers, for the host's tests of those wrappers.
//!
//! Text commands; errors reply `err:<error>`:
//! - `config:<key>`: the value, or `none`.
//! - `config-all`: `key=value` pairs joined by `,`.
//! - `set:<bucket>:<key>:<value>`, `get:<bucket>:<key>` (or `none`),
//!   `delete:<bucket>:<key>`, `exists:<bucket>:<key>`.
//! - `incr:<bucket>:<key>:<delta>`: the new count.
//! - `keys:<bucket>`: every key, joined by `,`.
//! - `fill:<bucket>:<n>`: set keys `k0000` to `k<n-1>` in one batch.
//! - `many:<bucket>:<k1,k2,...>`: the values, `-` for missing, joined by `,`.
//! - `drop-many:<bucket>:<k1,k2,...>`: delete them in one batch.
//! - `point:<bucket>:<key>:<x>:<y>`: store a `Point` as CBOR, read it back
//!   and reply `x,y`.
//! - `point-bytes:<bucket>:<key>`: read the key as a `Point` (to provoke a
//!   decode error).

use paddock_sdk::kv::{Bucket, KvError};
use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Point {
    x: i64,
    y: i64,
}

#[derive(Default)]
struct Fixture;

fn kv(command: &str) -> Result<String, KvError> {
    let mut parts = command.splitn(4, ':');
    let verb = parts.next().unwrap_or("");
    let bucket = Bucket::open(parts.next().unwrap_or(""))?;
    let key = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");
    let list = || key.split(',').map(str::to_string).collect::<Vec<_>>();
    Ok(match verb {
        "set" => {
            bucket.set(key, rest.as_bytes())?;
            "ok".into()
        }
        "get" => bucket.get(key)?.map_or_else(
            || "none".into(),
            |v| String::from_utf8_lossy(&v).into_owned(),
        ),
        "delete" => {
            bucket.delete(key)?;
            "ok".into()
        }
        "exists" => bucket.exists(key)?.to_string(),
        "incr" => bucket
            .increment(key, rest.parse().unwrap_or(1))?
            .to_string(),
        "keys" => bucket.keys()?.join(","),
        "fill" => {
            let n: usize = key.parse().unwrap_or(0);
            let entries: Vec<(String, Vec<u8>)> = (0..n)
                .map(|i| (format!("k{i:04}"), b"v".to_vec()))
                .collect();
            bucket.set_many(&entries)?;
            "ok".into()
        }
        "many" => bucket
            .get_many(&list())?
            .into_iter()
            .map(|v| v.map_or_else(|| "-".into(), |v| String::from_utf8_lossy(&v).into_owned()))
            .collect::<Vec<String>>()
            .join(","),
        "drop-many" => {
            bucket.delete_many(&list())?;
            "ok".into()
        }
        "point" => {
            let (x, y) = rest.split_once(':').unwrap_or(("0", "0"));
            let point = Point {
                x: x.parse().unwrap_or(0),
                y: y.parse().unwrap_or(0),
            };
            bucket.set_value(key, &point)?;
            let back: Point = bucket
                .get_value(key)?
                .ok_or_else(|| KvError::Other("missing".into()))?;
            format!("{},{}", back.x, back.y)
        }
        "point-bytes" => {
            let back: Option<Point> = bucket.get_value(key)?;
            format!("{back:?}")
        }
        other => format!("unknown command {other}"),
    })
}

fn config(command: &str) -> Result<String, paddock_sdk::config::ConfigError> {
    Ok(match command.split_once(':') {
        Some(("config", key)) => paddock_sdk::config::get(key)?.unwrap_or_else(|| "none".into()),
        _ => paddock_sdk::config::get_all()?
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(","),
    })
}

impl App for Fixture {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self)
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let command = msg.as_text().unwrap_or("");
        let reply = if command.starts_with("config") {
            config(command).unwrap_or_else(|e| format!("err:{e}"))
        } else {
            kv(command).unwrap_or_else(|e| format!("err:{e}"))
        };
        Reply::Now(Envelope::text(reply))
    }
}

paddock_sdk::app!(Fixture);
