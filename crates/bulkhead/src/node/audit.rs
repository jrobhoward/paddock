//! The audit log: one JSON object per line, appended for every deploy,
//! upgrade, rollback, stop, start, removal, kill, grant and refusal.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::events::Events;

/// Appends audit records, and sends each to the node's event followers.
#[derive(Debug)]
pub(crate) struct Audit {
    path: PathBuf,
    events: Events,
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl Audit {
    pub(crate) fn new(path: PathBuf, events: Events) -> Self {
        Self { path, events }
    }

    /// Append `{"time_ms", "event", ...fields}`. Each record is one
    /// `O_APPEND` write, so concurrent writers do not interleave lines. A
    /// failure to write is logged, not returned: the operation it records
    /// has already happened.
    pub(crate) fn record(&self, event: &str, fields: Value) {
        self.events.emit("audit", event, fields.clone());
        let mut record = Map::new();
        record.insert("time_ms".into(), json!(now_ms()));
        record.insert("event".into(), json!(event));
        if let Value::Object(more) = fields {
            record.extend(more);
        }
        let mut line = Value::Object(record).to_string();
        line.push('\n');
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = written {
            tracing::error!(path = %self.path.display(), error = %e, "cannot write audit record");
        }
    }
}
