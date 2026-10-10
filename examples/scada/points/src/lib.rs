//! The point table: a pool of instances, each device's updates routed to
//! one of them by `hash:device`, keeping every point in the shared `points`
//! bucket under `<device>/<table>/<address>` and each device's state under
//! `<device>/status`. One update is one batch write, so a scan costs one
//! transaction.
//!
//! One instance also registers `points-api` and answers queries (any
//! instance can: the table is in the bucket):
//!
//! - `get <device> <table> <address>`: the value, or `none`.
//! - `status <device>`: `online`, `offline: <why>`, or `unknown`.
//! - `set <device> holding|coil <address> <value>`: write to the device
//!   through the Modbus extension; `ok` or `err:<why>`.
//! - `updates`: how many updates this instance has stored.

use paddock_sdk::kv::Bucket;
use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};
use serde::Deserialize;

#[allow(missing_docs)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../crates/paddock-modbus/wit",
        world: "paddock-modbus:modbus/modbus",
    });
}

use bindings::paddock_modbus::modbus::client;

/// An update from the Modbus extension (`paddock_modbus::points`).
#[derive(Debug, Deserialize)]
struct Update {
    device: String,
    kind: String,
    #[serde(default)]
    points: Vec<Point>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Point {
    table: String,
    address: u16,
    value: u16,
}

#[derive(Default)]
struct Points {
    updates: u64,
}

fn board() -> Result<Bucket, String> {
    Bucket::open("points").map_err(|e| e.to_string())
}

fn store(update: &Update) -> Result<(), String> {
    let board = board()?;
    let mut entries: Vec<(String, Vec<u8>)> = update
        .points
        .iter()
        .map(|p| {
            let key = format!("{}/{}/{}", update.device, p.table, p.address);
            (key, p.value.to_string().into_bytes())
        })
        .collect();
    let status = match update.kind.as_str() {
        "offline" => format!("offline: {}", update.error.as_deref().unwrap_or("")),
        _ => "online".to_string(),
    };
    entries.push((format!("{}/status", update.device), status.into_bytes()));
    board.set_many(&entries).map_err(|e| e.to_string())
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

fn read(key: &str, missing: &str) -> String {
    match board().and_then(|b| b.get(key).map_err(|e| e.to_string())) {
        Ok(Some(v)) => String::from_utf8_lossy(&v).into_owned(),
        Ok(None) => missing.to_string(),
        Err(e) => format!("err:{e}"),
    }
}

fn set(device: &str, table: &str, address: &str, value: &str) -> String {
    let (Ok(address), Ok(value)) = (address.parse::<u16>(), value.parse::<u16>()) else {
        return "err:address and value must be numbers".into();
    };
    let result = match table {
        "holding" => client::write_holding(device, address, &[value]),
        "coil" => client::write_coils(device, address, &[value != 0]),
        _ => return "err:table must be holding or coil".into(),
    };
    result.map_or_else(|e| format!("err:{}", show(&e)), |()| "ok".into())
}

impl App for Points {
    fn init(ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        // One instance gets the name; the others find it taken.
        let _ = ctx.register("points-api");
        Ok(Self::default())
    }

    fn handle_cast(&mut self, _ctx: &mut Ctx, msg: Envelope) {
        let Ok(update) = msg.decode::<Update>() else {
            paddock_sdk::warn!("not a modbus update");
            return;
        };
        match store(&update) {
            Ok(()) => self.updates += 1,
            Err(e) => paddock_sdk::warn!("storing {}: {e}", update.device),
        }
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let words: Vec<&str> = text.split_whitespace().collect();
        let reply = match words.as_slice() {
            ["get", device, table, address] => read(&format!("{device}/{table}/{address}"), "none"),
            ["status", device] => read(&format!("{device}/status"), "unknown"),
            ["set", device, table, address, value] => set(device, table, address, value),
            ["updates"] => self.updates.to_string(),
            _ => "commands: get <device> <table> <address>, status <device>, \
                  set <device> holding|coil <address> <value>, updates"
                .into(),
        };
        Reply::Now(Envelope::text(reply))
    }
}

paddock_sdk::app!(Points);
