//! Recent guest log records, kept in memory for `paddock logs`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use crate::api::LogLine;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};

use super::audit::now_ms;

/// The most recent guest records across all apps.
#[derive(Debug)]
pub struct LogRing {
    lines: Mutex<VecDeque<(String, LogLine)>>,
    capacity: usize,
}

impl LogRing {
    /// A ring keeping the last `capacity` records.
    #[must_use]
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            lines: Mutex::new(VecDeque::with_capacity(capacity.min(1 << 16))),
            capacity: capacity.max(1),
        })
    }

    fn push(&self, app: String, line: LogLine) {
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == self.capacity {
            lines.pop_front();
        }
        lines.push_back((app, line));
    }

    /// `app`'s last `n` records, oldest first.
    #[must_use]
    pub fn recent(&self, app: &str, n: usize) -> Vec<LogLine> {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
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
