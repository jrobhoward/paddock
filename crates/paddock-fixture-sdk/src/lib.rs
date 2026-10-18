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
//! - `latest:<bucket>:<key>`: as `entry`, read linearizably.
//! - `rev:<bucket>`: the bucket's revision.
//! - `cas:<bucket>:<key>:<expected or ->:<value>`: the new revision.
//! - `cad:<bucket>:<key>:<expected>`: the bucket's new revision.
//! - `counter:<name>:<label>:<n>`, `gauge:<name>:<label>:<n>`,
//!   `observe:<name>:<label>:<n>` (a histogram with buckets 1, 10): make the
//!   metric with label `l=<label>` and update it; `ok` or `err:<error>`.
//! - `secret:<name>`: the secret as text, or `err:<error>`.
//! - `fetch <method> <url> [<body>]` (space separated): an outgoing HTTP
//!   request (`paddock_sdk::http_client`); replies `<status> <body>`, or
//!   `err:<error>`.
//! - `produce:<app>:<n>:<batch>`: send `n` sample points to the app
//!   (`whereis`), one cast each with `batch` 0, else in batches of `batch`
//!   through `paddock_sdk::batch`; replies `ok`. Casts of points (single
//!   or batched) are counted, not kept: `points` replies the count.
//! - `produce-timed:<app>:<n>:<batch>:<delay_ms>`: push `n` points into a
//!   batcher with that delay and send only full batches; the rest goes out
//!   when the batcher's timer fires. Replies `ok`.
//! - `busy:<ms>`: spin for that long, then `ok` (notices queue meanwhile).
//! - HTTP requests (`paddock_sdk::http`), by the end of the path: `/plain`
//!   replies a plain text envelope; `/status/<n>` replies status n;
//!   `/crash` traps; `/never` never replies; `/big/<n>` replies n bytes; anything else replies 200 with
//!   `<method> <path>?<query> key=<key> body=<body> scheme=<scheme> pid=<pid>`.
//! - `changes`: every change notice received, `<key>@<revision>`, joined
//!   by `,`.
//! - SQL (`paddock:sql`), fields separated by `\x1f` (statements contain
//!   colons): `sql\x1fquery\x1f<ds>\x1f<statement>\x1f<param>...` replies
//!   `columns|rows` in debug form, `sql\x1fexec\x1f<ds>\x1f...` the rows
//!   changed; `sql\x1fbegin\x1f<ds>`, then `sql\x1ftx-query\x1f<statement>...`,
//!   `sql\x1ftx-exec\x1f...`, `sql\x1fcommit`, `sql\x1frollback`, or
//!   `sql\x1fdrop` (drop the transaction unended). Parameters are typed by
//!   a prefix: `i:` integer, `f:` float, `t:` text, `b:` boolean, `x:` hex
//!   bytes, `null`. Errors reply `err:<debug>`.

use paddock_sdk::groups::{self, GroupError};
use paddock_sdk::http::{Request, Response};
use paddock_sdk::kv::{Bucket, KvError};
use paddock_sdk::watch::{self, Change, WatchError};
use paddock_sdk::{App, CallRef, Ctx, Envelope, Info, Reply};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Point {
    x: i64,
    y: i64,
}

/// A sample point, as a protocol extension might report it.
#[derive(Debug, Serialize, Deserialize)]
struct Sample {
    device: u32,
    address: u16,
    value: f64,
    time_ms: u64,
}

/// The schema of a single [`Sample`] cast.
const SAMPLE: &str = "fixture/sample";

/// `produce:<app>:<n>:<batch>`.
fn produce(ctx: &mut Ctx, args: &str) -> String {
    use paddock_sdk::batch::{Batcher, Limits};

    let mut parts = args.split(':');
    let app = parts.next().unwrap_or("");
    let n: u32 = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    let batch: usize = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    let Some(to) = ctx.whereis(app) else {
        return format!("err:no {app}");
    };
    let sample = |i: u32| Sample {
        device: i % 64,
        address: u16::try_from(i % 1000).unwrap_or(0),
        value: f64::from(i) * 0.5,
        time_ms: u64::from(i),
    };
    if batch == 0 {
        for i in 0..n {
            match Envelope::cbor(&sample(i)) {
                Ok(msg) => ctx.cast(to, msg.with_schema(SAMPLE)),
                Err(e) => return format!("err:{e}"),
            }
        }
        return "ok".into();
    }
    let mut out = Batcher::new(Limits {
        max_items: batch,
        max_bytes: 1 << 20,
        max_delay: None,
    });
    for i in 0..n {
        match out.push(ctx, to, &sample(i)) {
            Ok(ready) => {
                for (to, msg) in ready {
                    ctx.cast(to, msg);
                }
            }
            Err(e) => return format!("err:{e}"),
        }
    }
    for (to, msg) in out.flush_all(ctx) {
        ctx.cast(to, msg);
    }
    "ok".into()
}

#[derive(Default)]
struct Fixture {
    /// Sample points received, single or batched.
    points: u64,
    /// Batches waiting for their timer, from `produce-timed`.
    timed: Option<paddock_sdk::batch::Batcher<paddock_sdk::Pid, Sample>>,
    received: Vec<String>,
    changes: Vec<String>,
    /// An open SQL transaction, from `sql begin`.
    tx: Option<paddock_sdk::sql::Transaction>,
    /// What `health` answers: healthy unless `sick:<why>` set a reason.
    sick: Option<String>,
}

/// `fetch <method> <url> [<body>]`.
fn fetch(args: &str) -> String {
    use paddock_sdk::http_client;

    let mut parts = args.splitn(3, ' ');
    let method = parts.next().unwrap_or("GET");
    let url = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    match http_client::send(&http_client::Request::new(method, url).body(body)) {
        Ok(r) => format!("{} {}", r.status, String::from_utf8_lossy(&r.body)),
        Err(e) => format!("err:{e}"),
    }
}

fn metric(command: &str) -> Result<(), paddock_sdk::metrics::MetricsError> {
    use paddock_sdk::metrics::{Counter, Gauge, Histogram};
    let mut parts = command.splitn(4, ':');
    let verb = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    let label = parts.next().unwrap_or("");
    let n: f64 = parts.next().and_then(|n| n.parse().ok()).unwrap_or(1.0);
    let labels = [("l", label)];
    match verb {
        "counter" => Counter::new(name, "A fixture counter.", &labels)?.add(n),
        "gauge" => Gauge::new(name, "A fixture gauge.", &labels)?.set(n),
        _ => Histogram::new(name, "A fixture histogram.", &labels, &[1.0, 10.0])?.observe(n),
    }
    Ok(())
}

fn http(ctx: &mut Ctx, req: &Request) -> Reply {
    let path = req.path.as_str();
    if path.ends_with("/plain") {
        return Reply::Now(Envelope::text("plain"));
    }
    if path.ends_with("/crash") {
        panic!("asked to crash");
    }
    if path.ends_with("/never") {
        return Reply::Later;
    }
    if let Some((_, n)) = path.rsplit_once("/big/") {
        let body = "x".repeat(n.parse().unwrap_or(0));
        return Reply::Now(Response::text(200, body).into());
    }
    if let Some((_, code)) = path.rsplit_once("/status/") {
        return Reply::Now(Response::new(code.parse().unwrap_or(500)).into());
    }
    let echo = format!(
        "{} {}?{} key={} body={} scheme={} pid={}",
        req.method,
        path,
        req.query.as_deref().unwrap_or(""),
        req.key.as_deref().unwrap_or("-"),
        req.text().unwrap_or("<binary>"),
        req.scheme,
        ctx.pid(),
    );
    Reply::Now(Response::text(200, echo).header("x-fixture", "sdk").into())
}

const WATCH_VERBS: [&str; 10] = [
    "watch",
    "watch-prefix",
    "resume",
    "resume-prefix",
    "unwatch",
    "entry",
    "latest",
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
        "latest" => watch::get_linearizable(bucket, key)?.map_or_else(
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

fn sql_param(p: &str) -> paddock_sdk::sql::Value {
    use paddock_sdk::sql::Value;
    if p == "null" {
        return Value::Null;
    }
    let (kind, v) = p.split_once(':').unwrap_or(("t", p));
    match kind {
        "i" => Value::Int(v.parse().unwrap_or(0)),
        "f" => Value::Float(v.parse().unwrap_or(0.0)),
        "b" => Value::Boolean(v == "true"),
        "x" => Value::Bytes(
            (0..v.len() / 2)
                .filter_map(|i| u8::from_str_radix(&v[i * 2..i * 2 + 2], 16).ok())
                .collect(),
        ),
        _ => Value::Text(v.to_string()),
    }
}

impl Fixture {
    /// A SQL command; see the module documentation.
    fn sql(&mut self, command: &str) -> String {
        use paddock_sdk::sql;
        let mut fields = command.split('\x1f');
        let verb = fields.next().unwrap_or("");
        let out = match verb {
            "query" | "exec" | "begin" => {
                let ds = fields.next().unwrap_or("");
                if verb == "begin" {
                    sql::begin(ds).map(|tx| {
                        self.tx = Some(tx);
                        "ok".to_string()
                    })
                } else {
                    let statement = fields.next().unwrap_or("");
                    let params: Vec<sql::Value> = fields.map(sql_param).collect();
                    if verb == "query" {
                        sql::query(ds, statement, &params)
                            .map(|r| format!("{:?}|{:?}", r.columns, r.rows))
                    } else {
                        sql::execute(ds, statement, &params).map(|n| n.to_string())
                    }
                }
            }
            "tx-query" | "tx-exec" => {
                let statement = fields.next().unwrap_or("");
                let params: Vec<sql::Value> = fields.map(sql_param).collect();
                match &self.tx {
                    None => return "err:no transaction".into(),
                    Some(tx) if verb == "tx-query" => tx
                        .query(statement, &params)
                        .map(|r| format!("{:?}|{:?}", r.columns, r.rows)),
                    Some(tx) => tx.execute(statement, &params).map(|n| n.to_string()),
                }
            }
            "commit" | "rollback" => match self.tx.take() {
                None => return "err:no transaction".into(),
                Some(tx) if verb == "commit" => tx.commit().map(|()| "ok".into()),
                Some(tx) => tx.rollback().map(|()| "ok".into()),
            },
            "drop" => {
                self.tx = None;
                Ok("ok".into())
            }
            other => return format!("err:unknown sql command {other}"),
        };
        out.unwrap_or_else(|e| format!("err:{e:?}"))
    }
}

impl Fixture {
    /// `produce-timed:<app>:<n>:<batch>:<delay_ms>`.
    fn produce_timed(&mut self, ctx: &mut Ctx, args: &str) -> String {
        use paddock_sdk::batch::{Batcher, Limits};

        let parts: Vec<&str> = args.split(':').collect();
        let [app, n, batch, delay] = parts.as_slice() else {
            return "err:usage".into();
        };
        let (Ok(n), Ok(batch), Ok(delay)) = (n.parse::<u32>(), batch.parse(), delay.parse()) else {
            return "err:usage".into();
        };
        let Some(to) = ctx.whereis(app) else {
            return format!("err:no {app}");
        };
        let out = self.timed.get_or_insert_with(|| {
            Batcher::new(Limits {
                max_items: batch,
                max_bytes: 1 << 20,
                max_delay: Some(std::time::Duration::from_millis(delay)),
            })
        });
        for i in 0..n {
            let point = Sample {
                device: i,
                address: 0,
                value: 0.0,
                time_ms: 0,
            };
            match out.push(ctx, to, &point) {
                Ok(ready) => {
                    for (to, msg) in ready {
                        ctx.cast(to, msg);
                    }
                }
                Err(e) => return format!("err:{e}"),
            }
        }
        "ok".into()
    }
}

impl App for Fixture {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self::default())
    }

    fn health(&mut self, _ctx: &mut Ctx) -> Result<(), String> {
        self.sick.clone().map_or(Ok(()), Err)
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        if let Some(req) = Request::from_envelope(&msg) {
            return http(ctx, &req);
        }
        let command = msg.as_text().unwrap_or("");
        if let Some(rest) = command.strip_prefix("fetch ") {
            return Reply::Now(Envelope::text(fetch(rest)));
        }
        if let Some(rest) = command.strip_prefix("produce:") {
            return Reply::Now(Envelope::text(produce(ctx, rest)));
        }
        if let Some(rest) = command.strip_prefix("produce-timed:") {
            return Reply::Now(Envelope::text(self.produce_timed(ctx, rest)));
        }
        if command == "points" {
            return Reply::Now(Envelope::text(self.points.to_string()));
        }
        if let Some(rest) = command.strip_prefix("sql\x1f") {
            return Reply::Now(Envelope::text(self.sql(rest)));
        }
        let verb = command.split(':').next().unwrap_or("");
        let reply = if command.starts_with("config") {
            config(command).unwrap_or_else(|e| format!("err:{e}"))
        } else if verb == "received" {
            self.received.join(",")
        } else if ["counter", "gauge", "observe"].contains(&verb) {
            metric(command).map_or_else(|e| format!("err:{e}"), |()| "ok".into())
        } else if verb == "secret" {
            let name = command.split_once(':').map_or("", |(_, n)| n);
            paddock_sdk::secrets::get_string(name).unwrap_or_else(|e| format!("err:{e}"))
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
        } else if verb == "sick" {
            self.sick = command.split_once(':').map(|(_, why)| why.to_string());
            "ok".into()
        } else if WATCH_VERBS.contains(&verb) {
            watching(command).unwrap_or_else(|e| format!("err:{e}"))
        } else if ["join", "leave", "members", "publish"].contains(&verb) {
            group(command).unwrap_or_else(|e| format!("err:{e}"))
        } else {
            kv(command).unwrap_or_else(|e| format!("err:{e}"))
        };
        Reply::Now(Envelope::text(reply))
    }

    /// The casts and change notices received, so they survive an upgrade
    /// or a passivation: each list's entries joined by a record separator,
    /// the two lists by a group separator.
    fn save_snapshot(&self) -> Result<Vec<u8>, String> {
        Ok(format!(
            "{}\u{1d}{}",
            self.received.join("\u{1e}"),
            self.changes.join("\u{1e}")
        )
        .into_bytes())
    }

    /// What `save_snapshot` wrote; an empty snapshot starts empty.
    fn load_snapshot(_ctx: &mut Ctx, _from: &str, snapshot: &[u8]) -> Result<Self, String> {
        let text = String::from_utf8(snapshot.to_vec()).map_err(|e| e.to_string())?;
        let list = |s: &str| -> Vec<String> {
            s.split('\u{1e}')
                .filter(|e| !e.is_empty())
                .map(str::to_string)
                .collect()
        };
        let (received, changes) = text.split_once('\u{1d}').unwrap_or(("", ""));
        Ok(Self {
            received: list(received),
            changes: list(changes),
            ..Self::default()
        })
    }

    fn handle_info(&mut self, ctx: &mut Ctx, info: Info) {
        if let (Info::Timer { timer, .. }, Some(out)) = (&info, self.timed.as_mut())
            && let Some((to, msg)) = out.on_timer(*timer)
        {
            ctx.cast(to, msg);
            return;
        }
        if let Some(change) = Change::from_info(&info) {
            self.changes
                .push(format!("{}@{}", change.key, change.revision));
        }
    }

    fn handle_cast(&mut self, _ctx: &mut Ctx, msg: Envelope) {
        if let Some(batch) = paddock_sdk::batch::items::<Sample>(&msg) {
            self.points += batch.map_or(0, |points| points.len() as u64);
            return;
        }
        if msg.schema.as_deref() == Some(SAMPLE) {
            self.points += u64::from(msg.decode::<Sample>().is_ok());
            return;
        }
        self.received
            .push(msg.as_text().unwrap_or("<binary>").to_string());
    }
}

paddock_sdk::app!(Fixture);
