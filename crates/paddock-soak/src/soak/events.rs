//! What the runner did, one JSON object per line in `events.jsonl`: the
//! process signals the checker needs, and the faults and checks a reader
//! needs to follow a failed run.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use super::time::now_us;

/// One thing the runner did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Event {
    /// When, in microseconds since the epoch. For a signal, the moment its
    /// effect is certain: after a killed process was reaped, after a
    /// stopped one shows as stopped, before a stopped one is continued.
    pub at_us: i64,
    /// The node it concerns; 0 for none.
    pub node: u32,
    /// What happened.
    pub what: What,
}

/// The kinds of event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum What {
    /// The node's process was started.
    Spawn,
    /// The node's process was killed with `SIGKILL` and reaped.
    Kill,
    /// The node's process was found to have exited on its own.
    Exited,
    /// The node's process was stopped with `SIGSTOP`.
    Freeze,
    /// The node's process is about to be continued with `SIGCONT`.
    Thaw,
    /// Frames from `node` to `to` are now treated as `mode`.
    Link { to: u32, mode: String },
    /// Everything healed, then the nodes running the singleton counted.
    Settled { runners: Vec<u32> },
    /// Anything else worth reading in a failed run.
    Note { text: String },
}

/// `events.jsonl`, appended to as things happen.
#[derive(Debug)]
pub(crate) struct EventLog {
    file: Mutex<File>,
}

impl EventLog {
    pub(crate) fn create(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Record `what` for `node` now.
    pub(crate) fn record(&self, node: u32, what: What) {
        self.record_at(now_us(), node, what);
    }

    pub(crate) fn record_at(&self, at_us: i64, node: u32, what: What) {
        let event = Event { at_us, node, what };
        let Ok(mut line) = serde_json::to_string(&event) else {
            return;
        };
        line.push('\n');
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        // A lost line weakens the check, so say so rather than stop the run.
        if let Err(e) = file.write_all(line.as_bytes()).and_then(|()| file.flush()) {
            eprintln!("events.jsonl: {e}");
        }
    }
}

/// Every event in `path`, in the order written.
pub(crate) fn read(path: &Path) -> anyhow::Result<Vec<Event>> {
    let file = File::open(path)?;
    let mut events = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str(&line)
            .map_err(|e| anyhow::anyhow!("{}:{}: {e}", path.display(), i + 1))?;
        events.push(event);
    }
    Ok(events)
}
