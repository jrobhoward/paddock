//! The node's event stream, for `GET /v1/events` and `paddock events`:
//! audit records as they are written, instance exits and
//! restarts, cluster members heard and lost, and failover changes. Each
//! event is one JSON object (`time_ms`, `kind`, `event`, and fields of its
//! own) on a line of its own; [`crate::api::Event`] reads them back.

use std::sync::Arc;

use paddock_host::InstanceEvent;
use paddock_host::transport::liveness::NodeEvent;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;

use super::audit::now_ms;

/// Events a follower may fall behind by before it loses the oldest.
const BACKLOG: usize = 1024;

/// Where the node's events go; cheap to clone.
#[derive(Debug, Clone)]
pub(crate) struct Events {
    tx: broadcast::Sender<Arc<str>>,
}

impl Events {
    pub(crate) fn new() -> Self {
        Self {
            tx: broadcast::channel(BACKLOG).0,
        }
    }

    /// Send `{"time_ms", "kind", "event", ...fields}` to every follower.
    /// Free when nobody follows.
    pub(crate) fn emit(&self, kind: &str, event: &str, fields: Value) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        let _ = self.tx.send(line(kind, event, fields).into());
    }

    /// Every event from now on, each a line of JSON without its newline.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.tx.subscribe()
    }
}

/// One event as a line of JSON, without the newline.
pub(crate) fn line(kind: &str, event: &str, fields: Value) -> String {
    let mut record = Map::new();
    record.insert("time_ms".into(), json!(now_ms()));
    record.insert("kind".into(), json!(kind));
    record.insert("event".into(), json!(event));
    if let Value::Object(more) = fields {
        record.extend(more);
    }
    Value::Object(record).to_string()
}

/// An instance's exit or restart as `(event, fields)`.
pub(crate) fn instance(e: &InstanceEvent) -> Option<(&'static str, Value)> {
    Some(match e {
        InstanceEvent::Exited { pid, app, reason } => (
            "exit",
            json!({ "pid": pid.text(), "app": app, "reason": reason.to_string() }),
        ),
        InstanceEvent::Restarted {
            pid,
            previous,
            child,
        } => (
            "restart",
            json!({ "pid": pid.text(), "previous": previous.text(), "child": child }),
        ),
        _ => return None,
    })
}

/// A change in what this node knows of another, as `(event, fields)`.
pub(crate) fn member(e: &NodeEvent) -> Option<(&'static str, Value)> {
    Some(match e {
        NodeEvent::Down { node, session } => ("down", json!({ "node": node, "session": session })),
        NodeEvent::Unreachable { node } => ("unreachable", json!({ "node": node })),
        NodeEvent::Reachable { node } => ("reachable", json!({ "node": node })),
        NodeEvent::Joined { node, creation } => {
            ("joined", json!({ "node": node, "creation": creation }))
        }
        NodeEvent::Left { node } => ("left", json!({ "node": node })),
        NodeEvent::PathSilent { node, path } => {
            ("path_silent", json!({ "node": node, "path": path }))
        }
        NodeEvent::PathHeard { node, path } => {
            ("path_heard", json!({ "node": node, "path": path }))
        }
        _ => return None,
    })
}

/// Forward the runtime's instance and member events to `events` until
/// `stop` turns true.
pub(crate) async fn forward(
    events: Events,
    mut instances: broadcast::Receiver<InstanceEvent>,
    mut members: broadcast::Receiver<NodeEvent>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    use broadcast::error::RecvError;

    let lagged = |n: u64| events.emit("node", "lagged", json!({ "missed": n }));
    loop {
        tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            got = instances.recv() => match got {
                Ok(e) => if let Some((event, fields)) = instance(&e) {
                    events.emit("instance", event, fields);
                },
                Err(RecvError::Lagged(n)) => lagged(n),
                Err(RecvError::Closed) => return,
            },
            got = members.recv() => match got {
                Ok(e) => if let Some((event, fields)) = member(&e) {
                    events.emit("member", event, fields);
                },
                Err(RecvError::Lagged(n)) => lagged(n),
                Err(RecvError::Closed) => return,
            },
        }
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod events_tests;
