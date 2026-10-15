//! Instance pools: one app's instances under its root supervisor, and how a
//! message picks one (`instances.routing`).

use std::sync::atomic::{AtomicUsize, Ordering};

use paddock_bundle::manifest::Routing;

use crate::instance::InstanceHandle;
use crate::message::Envelope;
use crate::supervisor::SupervisorHandle;

/// An app's running instances, as its root supervisor reports them.
#[derive(Debug)]
pub(crate) struct Pool {
    app: String,
    routing: Routing,
    supervisor: SupervisorHandle,
    next: AtomicUsize,
}

impl Pool {
    pub(crate) fn new(app: String, routing: Routing, supervisor: SupervisorHandle) -> Self {
        Self {
            app,
            routing,
            supervisor,
            next: AtomicUsize::new(0),
        }
    }

    /// The app's running instances, in child order. Positions stay put
    /// across restarts, so hash routing is stable while the pool size is.
    pub(crate) fn members(&self) -> Vec<InstanceHandle> {
        self.supervisor
            .children()
            .iter()
            .filter(|c| c.app.as_deref() == Some(self.app.as_str()))
            .filter_map(|c| c.instance().cloned())
            .collect()
    }

    /// Pick one of the members `live` accepts, round-robin.
    pub(crate) fn pick_live(
        &self,
        live: impl Fn(&InstanceHandle) -> bool,
    ) -> Option<InstanceHandle> {
        let members: Vec<InstanceHandle> = self.members().into_iter().filter(|h| live(h)).collect();
        let index = pick_index(members.len(), None, &self.next)?;
        members.into_iter().nth(index)
    }

    /// Pick an instance for `msg` (round-robin when there is no message, or
    /// no routing key in it).
    pub(crate) fn pick(&self, msg: Option<&Envelope>) -> Option<InstanceHandle> {
        let members = self.members();
        let key = match (&self.routing, msg) {
            (Routing::Hash(field), Some(msg)) => routing_key(msg, field),
            _ => None,
        };
        let index = pick_index(members.len(), key.as_deref(), &self.next)?;
        members.into_iter().nth(index)
    }
}

/// Which of `n` members gets a message: by hash of `key` if there is one,
/// otherwise the next in turn.
pub(crate) fn pick_index(n: usize, key: Option<&[u8]>, next: &AtomicUsize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let hash = match key {
        Some(key) => fnv1a(key),
        None => u64::try_from(next.fetch_add(1, Ordering::Relaxed)).unwrap_or(0),
    };
    usize::try_from(hash % u64::try_from(n).unwrap_or(1)).ok()
}

/// 64-bit FNV-1a: stable across runs and nodes, unlike std's hasher.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The routing key: the top-level field `field` of a JSON or CBOR map body,
/// as bytes. Strings give their UTF-8, integers their decimal digits, byte
/// strings their bytes, so the same key routes the same way in either
/// encoding. `None` for other content types, other shapes or a missing field.
pub fn routing_key(msg: &Envelope, field: &str) -> Option<Vec<u8>> {
    let media = msg.content_type.split(';').next().unwrap_or("").trim();
    match media {
        "application/json" => {
            let value: serde_json::Value = serde_json::from_slice(&msg.body).ok()?;
            match value.get(field)? {
                serde_json::Value::String(s) => Some(s.clone().into_bytes()),
                serde_json::Value::Number(n) => Some(n.to_string().into_bytes()),
                _ => None,
            }
        }
        "application/cbor" => {
            let value: ciborium::Value = ciborium::from_reader(msg.body.as_slice()).ok()?;
            let map = value.as_map()?;
            let (_, found) = map.iter().find(|(k, _)| k.as_text() == Some(field))?;
            match found {
                ciborium::Value::Text(s) => Some(s.clone().into_bytes()),
                ciborium::Value::Integer(i) => Some(i128::from(*i).to_string().into_bytes()),
                ciborium::Value::Bytes(b) => Some(b.clone()),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod pool_tests;
