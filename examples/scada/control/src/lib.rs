//! Alarms, operator writes and the event log: a singleton, so on an HA
//! pair it runs on the primary only, and the standby's HTTP ingress
//! forwards its routes there (`[http.<name>] standby = "forward"`).
//!
//! It watches the shared `points` bucket that the `points` app keeps and
//! raises alarms from it: one per rule in its `rules` config (a point above
//! or below a limit), and one per device that goes offline
//! (`<device>-offline`). An alarm is `normal`, `active` (not yet
//! acknowledged), `acked` (active and acknowledged), or `cleared` (back to
//! normal but not acknowledged), as in ISA-18.2's simplest form. Alarms are
//! kept in the app's `alarms` bucket and every change in its `events`
//! bucket, the last 1000 of them, both on disk on the node it runs on.
//!
//! Before replicated buckets (ROADMAP K3), a failover or switchover starts
//! it on the other node from that node's own buckets: alarms that changed
//! meanwhile are re-raised from the table, and their acknowledgements and
//! the events of the other node are not there.
//!
//! Every response carries `x-paddock-pid` and `x-scada-version`.
//!
//! - `GET /scada/alarms`: `{"version", "pid", "alarms": {name: alarm}}`.
//! - `POST /scada/alarms/<name>/ack`: acknowledge; the alarm, 404 for an
//!   unknown one, 409 when there is nothing to acknowledge.
//! - `GET /scada/events?after=N`: `{"events": [...], "next": n}`, events
//!   numbered from 0, after `N` if given.
//! - `POST /scada/write`, body `{"device", "table": "holding" | "coil",
//!   "address", "value"}`: write to the device through the Modbus
//!   extension; `{"ok": true}`, or 502 with `{"error"}`.
//!
//! - v1 (`paddock.toml`) is the version above. It reads newer snapshot
//!   formats by ignoring the fields it does not know, so a rollback from v2
//!   keeps the alarms (later versions only add fields).
//! - v2 (`--features v2`, `paddock-v2.toml`) also counts how often each
//!   alarm was raised (`raised`), and migrates v1's snapshot (format 1)
//!   by counting an alarm active at the upgrade as raised once.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use paddock_sdk::http::{Request, Response};
use paddock_sdk::kv::Bucket;
use paddock_sdk::watch::{self, Change};
use paddock_sdk::{App, CallRef, Ctx, Envelope, Info, Reply, Snapshot, config};
use serde::{Deserialize, Serialize};

#[allow(missing_docs)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../crates/paddock-modbus/wit",
        world: "paddock-modbus:modbus/modbus",
    });
}

use bindings::paddock_modbus::modbus::client;

#[cfg(not(feature = "v2"))]
const VERSION: &str = "1.0.0";
#[cfg(feature = "v2")]
const VERSION: &str = "2.0.0";

/// The shared bucket the `points` app writes.
const POINTS: &str = "points";
/// This app's buckets.
const ALARMS: &str = "alarms";
const EVENTS: &str = "events";
/// Events kept; older ones are deleted as new ones are written.
const KEEP_EVENTS: u64 = 1000;

/// An alarm's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Normal,
    /// Active, not acknowledged.
    Active,
    /// Active and acknowledged.
    Acked,
    /// Back to normal, not acknowledged.
    Cleared,
}

/// One alarm.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Alarm {
    status: Status,
    /// The point's value at the last change, if it has one.
    value: Option<String>,
    /// When it last changed, in ms since the Unix epoch.
    changed_ms: u64,
    /// How often it was raised (v2). Absent from v1's records in the
    /// `alarms` bucket.
    #[cfg(feature = "v2")]
    #[serde(default)]
    raised: u64,
}

impl Alarm {
    fn new() -> Self {
        Self {
            status: Status::Normal,
            value: None,
            changed_ms: 0,
            #[cfg(feature = "v2")]
            raised: 0,
        }
    }
}

/// The state carried across upgrades: v1 writes format 1, and reads newer
/// formats by ignoring what it does not know, so a rollback keeps state.
#[cfg(not(feature = "v2"))]
#[derive(Debug, Default, Serialize, Deserialize, Snapshot)]
#[snapshot(version = 1, migrate = newer)]
struct State {
    alarms: BTreeMap<String, Alarm>,
    next_event: u64,
}

/// A newer format, read as format 1: later versions only add fields.
#[cfg(not(feature = "v2"))]
fn newer(from: u32, old: paddock_sdk::snapshot::Value) -> Result<State, String> {
    if from < 1 {
        return Err(format!("unknown snapshot format {from}"));
    }
    old.deserialized().map_err(|e| e.to_string())
}

/// v2 writes format 2 and reads format 1.
#[cfg(feature = "v2")]
#[derive(Debug, Default, Serialize, Deserialize, Snapshot)]
#[snapshot(version = 2, migrate = migrate)]
struct State {
    alarms: BTreeMap<String, Alarm>,
    next_event: u64,
}

#[cfg(feature = "v2")]
fn migrate(from: u32, old: paddock_sdk::snapshot::Value) -> Result<State, String> {
    #[derive(Deserialize)]
    struct AlarmV1 {
        status: Status,
        value: Option<String>,
        changed_ms: u64,
    }
    #[derive(Deserialize)]
    struct V1 {
        alarms: BTreeMap<String, AlarmV1>,
        next_event: u64,
    }
    if from != 1 {
        return Err(format!("unknown snapshot format {from}"));
    }
    let v1: V1 = old.deserialized().map_err(|e| e.to_string())?;
    let alarms = v1
        .alarms
        .into_iter()
        .map(|(name, a)| {
            let raised = u64::from(matches!(a.status, Status::Active | Status::Acked));
            let alarm = Alarm {
                status: a.status,
                value: a.value,
                changed_ms: a.changed_ms,
                raised,
            };
            (name, alarm)
        })
        .collect();
    Ok(State {
        alarms,
        next_event: v1.next_event,
    })
}

/// A rule from config: `name: key > limit` or `name: key < limit`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    name: String,
    key: String,
    above: bool,
    limit: u16,
}

/// Parse the `rules` config: rules separated by commas.
fn parse_rules(text: &str) -> Result<Vec<Rule>, String> {
    let mut rules = Vec::new();
    for rule in text.split(',').map(str::trim).filter(|r| !r.is_empty()) {
        let bad = || format!("rule {rule:?}: expected `name: key > limit` or `name: key < limit`");
        let (name, test) = rule.split_once(':').ok_or_else(bad)?;
        let (key, limit, above) = match (test.split_once('>'), test.split_once('<')) {
            (Some((k, l)), None) => (k, l, true),
            (None, Some((k, l))) => (k, l, false),
            _ => return Err(bad()),
        };
        rules.push(Rule {
            name: name.trim().to_string(),
            key: key.trim().to_string(),
            above,
            limit: limit.trim().parse().map_err(|_| bad())?,
        });
    }
    Ok(rules)
}

/// One entry in the event log.
#[derive(Debug, Serialize, Deserialize)]
struct Event {
    seq: u64,
    at_ms: u64,
    /// `started`, `raised`, `cleared`, `acked`, `normal` or `write`.
    kind: String,
    /// The alarm, device or version it is about.
    name: String,
    detail: String,
}

/// A device write, from `POST /scada/write`.
#[derive(Debug, Deserialize)]
struct Write {
    device: String,
    table: String,
    address: u16,
    value: u16,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn event_key(seq: u64) -> String {
    format!("{seq:020}")
}

fn open(name: &str) -> Result<Bucket, String> {
    Bucket::open(name).map_err(|e| format!("bucket {name}: {e}"))
}

fn show(e: &client::Error) -> String {
    match e {
        client::Error::AccessDenied => "access-denied".into(),
        client::Error::NoSuchDevice => "no-such-device".into(),
        client::Error::Offline(why) => format!("offline: {why}"),
        client::Error::Exception(code) => format!("exception {code}"),
        client::Error::Invalid(why) => format!("invalid: {why}"),
    }
}

struct Control {
    state: State,
    rules: Vec<Rule>,
}

impl Control {
    /// The rules from config, and the state from this node's buckets.
    fn open() -> Result<Self, String> {
        let rules = match config::get("rules").map_err(|e| format!("config: {e}"))? {
            Some(text) => parse_rules(&text)?,
            None => Vec::new(),
        };
        let mut state = State::default();
        let alarms = open(ALARMS)?;
        for name in alarms.keys().map_err(|e| e.to_string())? {
            if let Some(alarm) = alarms
                .get_value::<Alarm>(&name)
                .map_err(|e| e.to_string())?
            {
                state.alarms.insert(name, alarm);
            }
        }
        let events = open(EVENTS)?;
        state.next_event = events
            .keys()
            .map_err(|e| e.to_string())?
            .iter()
            .filter_map(|k| k.parse::<u64>().ok())
            .max()
            .map_or(0, |last| last + 1);
        Ok(Self { state, rules })
    }

    fn log(&mut self, kind: &str, name: &str, detail: String) {
        let seq = self.state.next_event;
        self.state.next_event += 1;
        let event = Event {
            seq,
            at_ms: now_ms(),
            kind: kind.into(),
            name: name.into(),
            detail,
        };
        let stored = open(EVENTS).and_then(|b| {
            b.set_value(&event_key(seq), &event)
                .map_err(|e| e.to_string())?;
            if seq >= KEEP_EVENTS {
                b.delete(&event_key(seq - KEEP_EVENTS))
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        });
        if let Err(e) = stored {
            paddock_sdk::warn!("event {seq} not stored: {e}");
        }
    }

    fn save(&self, name: &str) {
        let Some(alarm) = self.state.alarms.get(name) else {
            return;
        };
        if let Err(e) =
            open(ALARMS).and_then(|b| b.set_value(name, alarm).map_err(|e| e.to_string()))
        {
            paddock_sdk::warn!("alarm {name} not stored: {e}");
        }
    }

    /// Move alarm `name` as its condition is now `active`.
    fn condition(&mut self, name: &str, active: bool, value: Option<&str>) {
        let alarm = self
            .state
            .alarms
            .entry(name.to_string())
            .or_insert_with(Alarm::new);
        let (next, kind) = match (alarm.status, active) {
            (Status::Normal | Status::Cleared, true) => (Status::Active, "raised"),
            (Status::Active, false) => (Status::Cleared, "cleared"),
            (Status::Acked, false) => (Status::Normal, "normal"),
            _ => return,
        };
        alarm.status = next;
        alarm.value = value.map(str::to_string);
        alarm.changed_ms = now_ms();
        #[cfg(feature = "v2")]
        if next == Status::Active {
            alarm.raised += 1;
        }
        self.save(name);
        self.log(kind, name, value.unwrap_or("").to_string());
    }

    /// Check the alarms on `key`, whose value is now `value`.
    fn evaluate(&mut self, key: &str, value: Option<&str>) {
        if let Some(device) = key.strip_suffix("/status") {
            let offline = value.is_some_and(|v| v.starts_with("offline"));
            self.condition(&format!("{device}-offline"), offline, value);
            return;
        }
        let number = value.and_then(|v| v.parse::<u16>().ok());
        let hits: Vec<(String, bool)> = self
            .rules
            .iter()
            .filter(|r| r.key == key)
            .map(|r| {
                let active =
                    number.is_some_and(|n| if r.above { n > r.limit } else { n < r.limit });
                (r.name.clone(), active)
            })
            .collect();
        for (name, active) in hits {
            self.condition(&name, active, value);
        }
    }

    /// Check every point in the table.
    fn evaluate_all(&mut self) -> Result<(), String> {
        let points = open(POINTS)?;
        let keys = points.keys().map_err(|e| e.to_string())?;
        let values = points.get_many(&keys).map_err(|e| e.to_string())?;
        for (key, value) in keys.iter().zip(values) {
            let value = value.map(|v| String::from_utf8_lossy(&v).into_owned());
            self.evaluate(key, value.as_deref());
        }
        Ok(())
    }

    fn ack(&mut self, name: &str) -> Response {
        let Some(alarm) = self.state.alarms.get_mut(name) else {
            return Response::text(404, format!("no alarm {name}"));
        };
        let (next, kind) = match alarm.status {
            Status::Active => (Status::Acked, "acked"),
            Status::Cleared => (Status::Normal, "normal"),
            Status::Normal | Status::Acked => {
                return Response::text(409, format!("alarm {name} has nothing to acknowledge"));
            }
        };
        alarm.status = next;
        alarm.changed_ms = now_ms();
        let body = Response::json(200, &*alarm);
        self.save(name);
        self.log(kind, name, "acknowledged".into());
        body.unwrap_or_else(|e| Response::text(500, e.to_string()))
    }

    fn write(&mut self, req: &Request) -> Response {
        let w: Write = match serde_json::from_slice(&req.body) {
            Ok(w) => w,
            Err(e) => return Response::text(400, format!("body: {e}")),
        };
        let result = match w.table.as_str() {
            "holding" => client::write_holding(&w.device, w.address, &[w.value]),
            "coil" => client::write_coils(&w.device, w.address, &[w.value != 0]),
            _ => return Response::text(400, "table must be holding or coil"),
        };
        let detail = format!("{} {} = {}", w.table, w.address, w.value);
        match result {
            Ok(()) => {
                self.log("write", &w.device, detail);
                Response::json(200, &serde_json::json!({ "ok": true }))
            }
            Err(e) => {
                let error = show(&e);
                self.log("write", &w.device, format!("{detail}: {error}"));
                Response::json(502, &serde_json::json!({ "error": error }))
            }
        }
        .unwrap_or_else(|e| Response::text(500, e.to_string()))
    }

    fn events(req: &Request, next: u64) -> Result<Response, String> {
        let after = req.param("after").and_then(|a| a.parse::<u64>().ok());
        let bucket = open(EVENTS)?;
        let mut keys: Vec<String> = bucket
            .keys()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|k| {
                k.parse::<u64>()
                    .is_ok_and(|seq| after.is_none_or(|a| seq > a))
            })
            .collect();
        keys.sort();
        let mut events = Vec::with_capacity(keys.len());
        for key in &keys {
            if let Some(event) = bucket.get_value::<Event>(key).map_err(|e| e.to_string())? {
                events.push(event);
            }
        }
        Response::json(200, &serde_json::json!({ "events": events, "next": next }))
            .map_err(|e| e.to_string())
    }

    fn serve(&mut self, ctx: &Ctx, req: &Request) -> Response {
        let path: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        let response = match (req.method.as_str(), path.as_slice()) {
            ("GET", ["scada", "alarms"]) => Response::json(
                200,
                &serde_json::json!({
                    "version": VERSION,
                    "pid": ctx.pid().to_string(),
                    "alarms": self.state.alarms,
                }),
            )
            .unwrap_or_else(|e| Response::text(500, e.to_string())),
            ("POST", ["scada", "alarms", name, "ack"]) => self.ack(name),
            ("GET", ["scada", "events"]) => {
                Self::events(req, self.state.next_event).unwrap_or_else(|e| Response::text(500, e))
            }
            ("POST", ["scada", "write"]) => self.write(req),
            _ => Response::text(
                404,
                "GET /scada/alarms, POST /scada/alarms/<name>/ack, \
                 GET /scada/events, POST /scada/write",
            ),
        };
        response
            .header("x-paddock-pid", ctx.pid().to_string())
            .header("x-scada-version", VERSION)
    }
}

impl App for Control {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        let mut control = Self::open()?;
        // The watch first: a write after it is a notice, so none is missed
        // between it and the full read.
        watch::watch_prefix(POINTS, "").map_err(|e| format!("watch {POINTS}: {e}"))?;
        control.log("started", VERSION, String::new());
        control.evaluate_all()?;
        Ok(control)
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let reply = match Request::from_envelope(&msg) {
            Some(req) => self.serve(ctx, &req).into(),
            None => Envelope::text("send HTTP requests to /scada/alarms"),
        };
        Reply::Now(reply)
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
        let Some(change) = Change::from_info(&info) else {
            return;
        };
        let value = open(POINTS)
            .and_then(|b| b.get(&change.key).map_err(|e| e.to_string()))
            .map(|v| v.map(|v| String::from_utf8_lossy(&v).into_owned()));
        match value {
            Ok(value) => self.evaluate(&change.key, value.as_deref()),
            Err(e) => paddock_sdk::warn!("reading {}: {e}", change.key),
        }
    }

    fn save_snapshot(&self) -> Result<Vec<u8>, String> {
        self.state.to_snapshot()
    }

    fn load_snapshot(_ctx: &mut Ctx, from_version: &str, snapshot: &[u8]) -> Result<Self, String> {
        // The watch is the instance's and carries over with its pid; the
        // rules are read again, so a new version's defaults apply.
        let mut control = Self::open()?;
        control.state = State::from_snapshot(snapshot)?;
        control.log("started", VERSION, format!("upgraded from {from_version}"));
        for name in control.state.alarms.keys().cloned().collect::<Vec<_>>() {
            control.save(&name);
        }
        Ok(control)
    }
}

paddock_sdk::app!(Control);
