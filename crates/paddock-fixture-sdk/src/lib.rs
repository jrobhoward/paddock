//! Test fixture: an app that uses `wasi:config`, `wasi:keyvalue` and
//! `paddock:groups` through `paddock-sdk`'s wrappers, for the host's tests
//! of those wrappers.
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
//! - `join:<group>`, `leave:<group>`: `ok`.
//! - `members:<group>`: the members' pids, joined by `,`.
//! - `publish:<group>:<text>`: how many members took it.
//! - `received`: the text of every cast received, joined by `,`.
//! - `watch:<bucket>:<key>`, `watch-prefix:<bucket>:<prefix>`: `<id>@<revision>`.
//! - `resume:<bucket>:<key>:<after>`, `resume-prefix:<bucket>:<prefix>:<after>`:
//!   as `watch`, replaying changes after `<after>`.
//! - `unwatch:<id>`: `ok`.
//! - `entry:<bucket>:<key>`: `<value>@<revision>` or `none`.
//! - `rev:<bucket>`: the bucket's revision.
//! - `cas:<bucket>:<key>:<expected or ->:<value>`: the new revision.
//! - `cad:<bucket>:<key>:<expected>`: the bucket's new revision.
//! - `busy:<ms>`: spin for that long, then `ok` (notices queue meanwhile).
//! - `changes`: every change notice received, `<key>@<revision>`, joined
//!   by `,`.

use paddock_sdk::groups::{self, GroupError};
use paddock_sdk::kv::{Bucket, KvError};
use paddock_sdk::watch::{self, Change, WatchError};
use paddock_sdk::{App, CallRef, Ctx, Envelope, Info, Reply};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Point {
    x: i64,
    y: i64,
}

#[derive(Default)]
struct Fixture {
    received: Vec<String>,
    changes: Vec<String>,
}

const WATCH_VERBS: [&str; 9] = [
    "watch",
    "watch-prefix",
    "resume",
    "resume-prefix",
    "unwatch",
    "entry",
    "rev",
    "cas",
    "cad",
];

fn watching(command: &str) -> Result<String, WatchError> {
    let mut parts = command.splitn(5, ':');
    let verb = parts.next().unwrap_or("");
    let bucket = parts.next().unwrap_or("");
    let key = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");
    let number = |s: &str| s.parse::<u64>().unwrap_or(0);
    let started = |s: watch::Started| format!("{}@{}", s.id, s.revision);
    Ok(match verb {
        "watch" => started(watch::watch(bucket, key)?),
        "watch-prefix" => started(watch::watch_prefix(bucket, key)?),
        "resume" => started(watch::resume(bucket, key, false, number(rest))?),
        "resume-prefix" => started(watch::resume(bucket, key, true, number(rest))?),
        "unwatch" => watch::unwatch(number(bucket)).map(|()| "ok".to_string())?,
        "entry" => watch::get(bucket, key)?.map_or_else(
            || "none".into(),
            |e| format!("{}@{}", String::from_utf8_lossy(&e.value), e.revision),
        ),
        "rev" => watch::revision(bucket)?.to_string(),
        "cas" => {
            let expected = (rest != "-").then(|| number(rest));
            let value = parts.next().unwrap_or("");
            watch::compare_and_swap(bucket, key, expected, value.as_bytes())?.to_string()
        }
        _ => watch::compare_and_delete(bucket, key, number(rest))?.to_string(),
    })
}

fn group(command: &str) -> Result<String, GroupError> {
    let mut parts = command.splitn(3, ':');
    let verb = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    Ok(match verb {
        "join" => groups::join(name).map(|()| "ok".into())?,
        "leave" => groups::leave(name).map(|()| "ok".into())?,
        "members" => groups::members(name)?
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
        _ => groups::publish(name, Envelope::text(parts.next().unwrap_or("")))?.to_string(),
    })
}

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
        Ok(Self::default())
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let command = msg.as_text().unwrap_or("");
        let verb = command.split(':').next().unwrap_or("");
        let reply = if command.starts_with("config") {
            config(command).unwrap_or_else(|e| format!("err:{e}"))
        } else if verb == "received" {
            self.received.join(",")
        } else if verb == "busy" {
            let ms = command
                .split(':')
                .nth(1)
                .and_then(|m| m.parse().ok())
                .unwrap_or(0);
            let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            while std::time::Instant::now() < until {
                std::hint::spin_loop();
            }
            "ok".into()
        } else if verb == "changes" {
            self.changes.join(",")
        } else if WATCH_VERBS.contains(&verb) {
            watching(command).unwrap_or_else(|e| format!("err:{e}"))
        } else if ["join", "leave", "members", "publish"].contains(&verb) {
            group(command).unwrap_or_else(|e| format!("err:{e}"))
        } else {
            kv(command).unwrap_or_else(|e| format!("err:{e}"))
        };
        Reply::Now(Envelope::text(reply))
    }

    /// Starts empty after an upgrade, for the groups upgrade test.
    fn load_snapshot(_ctx: &mut Ctx, _from: &str, _snapshot: &[u8]) -> Result<Self, String> {
        Ok(Self::default())
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
        if let Some(change) = Change::from_info(&info) {
            self.changes
                .push(format!("{}@{}", change.key, change.revision));
        }
    }

    fn handle_cast(&mut self, _ctx: &mut Ctx, msg: Envelope) {
        self.received
            .push(msg.as_text().unwrap_or("<binary>").to_string());
    }
}

paddock_sdk::app!(Fixture);
