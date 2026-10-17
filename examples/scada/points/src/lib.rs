//! The point table: a pool of instances, each device's updates routed to
//! one of them by `hash:device`, keeping every point in the shared `points`
//! bucket under `<device>/<table>/<address>` and each device's state under
//! `<device>/status` (`online`, or `offline: <why>`). One update is one
//! batch write, so a scan costs one transaction. The `hmi` app serves the
//! table and `control` raises alarms from it (see the README).
//!
//! A call with `updates` replies how many updates this instance has
//! stored.

use paddock_sdk::kv::Bucket;
use paddock_sdk::{App, CallRef, Ctx, Envelope, Reply};
use serde::Deserialize;

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

impl App for Points {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
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
        let reply = match msg.as_text() {
            Some("updates") => self.updates.to_string(),
            _ => "commands: updates".into(),
        };
        Reply::Now(Envelope::text(reply))
    }
}

paddock_sdk::app!(Points);
