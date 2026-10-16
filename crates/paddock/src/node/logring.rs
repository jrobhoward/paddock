//! Recent guest log records, kept in memory for `paddock logs`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use crate::api::LogLine;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};

use super::audit::now_ms;

/// The most recent guest records across all apps, and a feed of new ones
/// for `paddock logs --follow`.
#[derive(Debug)]
pub struct LogRing {
    lines: Mutex<VecDeque<(String, LogLine)>>,
    capacity: usize,
    feed: broadcast::Sender<Arc<(String, LogLine)>>,
}

/// New log records of every app, each with its app's name.
pub(crate) type LogFeed = broadcast::Receiver<Arc<(String, LogLine)>>;

/// Records a follower may fall behind by before it loses the oldest.
const FEED_BACKLOG: usize = 4096;

impl LogRing {
    /// A ring keeping the last `capacity` records.
    #[must_use]
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            lines: Mutex::new(VecDeque::with_capacity(capacity.min(1 << 16))),
            capacity: capacity.max(1),
            feed: broadcast::channel(FEED_BACKLOG).0,
        })
    }

    fn push(&self, app: String, line: LogLine) {
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == self.capacity {
            lines.pop_front();
        }
        // Sent under the lock, so a follower's backlog from `follow` and
        // its feed neither overlap nor leave a gap. A broadcast send never
        // blocks.
        if self.feed.receiver_count() > 0 {
            let _ = self.feed.send(Arc::new((app.clone(), line.clone())));
        }
        lines.push_back((app, line));
    }

    /// `app`'s last `n` records, oldest first.
    #[must_use]
    pub fn recent(&self, app: &str, n: usize) -> Vec<LogLine> {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        Self::last(&lines, app, n)
    }

    /// `app`'s last `n` records, and every record of any app from then on
    /// (filter by the app name in each).
    pub(crate) fn follow(&self, app: &str, n: usize) -> (Vec<LogLine>, LogFeed) {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        (Self::last(&lines, app, n), self.feed.subscribe())
    }

    fn last(lines: &VecDeque<(String, LogLine)>, app: &str, n: usize) -> Vec<LogLine> {
        let mut found: Vec<LogLine> = lines
            .iter()
            .rev()
            .filter(|(a, _)| a == app)
            .take(n)
            .map(|(_, l)| l.clone())
            .collect();
        found.reverse();
        found
    }
}

/// A `tracing` layer that copies guest records (target `paddock::guest`)
/// into a [`LogRing`].
#[derive(Debug)]
pub struct RingLayer {
    ring: Arc<LogRing>,
}

impl RingLayer {
    /// A layer feeding `ring`.
    #[must_use]
    pub fn new(ring: Arc<LogRing>) -> Self {
        Self { ring }
    }
}

#[derive(Default)]
struct Fields {
    app: String,
    pid: String,
    source: String,
    message: String,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.set(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.set(field.name(), format!("{value:?}"));
    }
}

impl Fields {
    fn set(&mut self, name: &str, value: String) {
        match name {
            "app" => self.app = value,
            "pid" => self.pid = value,
            "stream" | "context" => self.source = value,
            "message" => self.message = value,
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "paddock::guest" {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let line = LogLine {
            time_ms: now_ms(),
            level: event.metadata().level().to_string(),
            pid: fields.pid,
            source: fields.source,
            message: fields.message,
        };
        self.ring.push(fields.app, line);
    }
}
