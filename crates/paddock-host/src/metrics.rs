//! Metrics: apps' own (`paddock:metrics`) and the host's, rendered in the
//! Prometheus text format ([`Metrics::render`]).
//!
//! App metrics are per app, not per instance: every instance of an app that
//! registers the same name and labels writes the same series, so counters
//! and histograms add up across a pool and survive restarts, and a gauge is
//! the last value any instance set. Exported names are
//! `<prefix>_<name>` (the manifest's `paddock:metrics` prefix) with an
//! `app` label. An app may hold at most its `max_series` series; past it,
//! registering fails.
//!
//! Host metrics count the events B1's probes report, per app: callbacks by
//! kind and their guest time, instance exits by reason, dropped messages,
//! and, read when rendered, instances, memory and mailbox depth. Each
//! instance holds its app's counters from the start, so recording is an
//! atomic add with no lookup. Names and labels are an interface, listed in
//! `docs/OBSERVABILITY.md`.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::instance::InstanceHandle;
use crate::message::ExitReason;

/// Histogram buckets when an app gives none, and for callback time (the
/// Prometheus client defaults, in seconds).
pub const DEFAULT_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Buckets for guest time per callback, in seconds.
const CALLBACK_BUCKETS: [f64; 10] = [
    0.000_01, 0.000_05, 0.000_1, 0.000_5, 0.001, 0.005, 0.01, 0.05, 0.1, 1.0,
];

/// A metric's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Only goes up.
    Counter,
    /// Set or moved either way.
    Gauge,
    /// Observations counted into buckets.
    Histogram,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// An `f64` updated without a lock.
#[derive(Debug, Default)]
struct AtomicF64(AtomicU64);

impl AtomicF64 {
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }

    fn add(&self, v: f64) {
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(current) + v).to_bits();
            match self
                .0
                .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(seen) => current = seen,
            }
        }
    }
}

/// Bucketed observations.
#[derive(Debug)]
struct Histogram {
    bounds: Vec<f64>,
    /// One count per bound, plus `+Inf`; not cumulative.
    counts: Vec<AtomicU64>,
    sum: AtomicF64,
}

impl Histogram {
    fn new(bounds: &[f64]) -> Self {
        Self {
            bounds: bounds.to_vec(),
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum: AtomicF64::default(),
        }
    }

    fn record(&self, v: f64) {
        let i = self
            .bounds
            .iter()
            .position(|b| v <= *b)
            .unwrap_or(self.bounds.len());
        if let Some(c) = self.counts.get(i) {
            c.fetch_add(1, Ordering::Relaxed);
        }
        self.sum.add(v);
    }

    fn write(&self, out: &mut String, name: &str, labels: &str) {
        let mut total = 0u64;
        for (i, count) in self.counts.iter().enumerate() {
            total += count.load(Ordering::Relaxed);
            let le = self
                .bounds
                .get(i)
                .map_or_else(|| "+Inf".to_string(), |b| format_f64(*b));
            let _ = writeln!(
                out,
                "{name}_bucket{{{labels}{}le=\"{le}\"}} {total}",
                comma(labels)
            );
        }
        let _ = writeln!(out, "{name}_sum{{{labels}}} {}", format_f64(self.sum.get()));
        let _ = writeln!(out, "{name}_count{{{labels}}} {total}");
    }
}

fn comma(labels: &str) -> &'static str {
    if labels.is_empty() { "" } else { "," }
}

fn format_f64(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

/// One series: a name with one set of label values.
#[derive(Debug)]
pub(crate) struct Series {
    kind: Kind,
    value: AtomicF64,
    histogram: Option<Histogram>,
}

/// Why an update was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The update does not fit the metric's kind, or the value is not a
    /// finite number (or is negative for a counter).
    Invalid(String),
}

impl Series {
    pub(crate) fn add(&self, delta: f64) -> Result<(), Refused> {
        match self.kind {
            Kind::Histogram => Err(Refused::Invalid("use record for a histogram".into())),
            _ if !delta.is_finite() => Err(Refused::Invalid("not a finite number".into())),
            Kind::Counter if delta < 0.0 => Err(Refused::Invalid("a counter only goes up".into())),
            Kind::Counter | Kind::Gauge => {
                self.value.add(delta);
                Ok(())
            }
        }
    }

    pub(crate) fn set(&self, value: f64) -> Result<(), Refused> {
        match self.kind {
            Kind::Gauge if value.is_finite() => {
                self.value.set(value);
                Ok(())
            }
            Kind::Gauge => Err(Refused::Invalid("not a finite number".into())),
            _ => Err(Refused::Invalid("only a gauge can be set".into())),
        }
    }

    pub(crate) fn record(&self, value: f64) -> Result<(), Refused> {
        match (&self.histogram, value.is_finite()) {
            (Some(h), true) => {
                h.record(value);
                Ok(())
            }
            (Some(_), false) => Err(Refused::Invalid("not a finite number".into())),
            (None, _) => Err(Refused::Invalid("only a histogram records".into())),
        }
    }
}

/// A family: one exported name, its kind and help, and its series by
/// label values.
#[derive(Debug)]
struct Family {
    kind: Kind,
    help: String,
    series: BTreeMap<Vec<(String, String)>, Arc<Series>>,
}

/// Why registering failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegisterError {
    /// The name or a label is not valid, or the name has another kind.
    Invalid(String),
    /// The app holds `max_series` series already.
    LimitReached,
}

/// One app's metrics.
#[derive(Debug, Default)]
pub(crate) struct AppMetrics {
    families: Mutex<BTreeMap<String, Family>>,
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl AppMetrics {
    /// The series `name` with `labels`, made if new. `name` is the exported
    /// name, prefix included.
    pub(crate) fn register(
        &self,
        kind: Kind,
        name: &str,
        help: &str,
        labels: &[(String, String)],
        buckets: &[f64],
        max_series: usize,
    ) -> Result<Arc<Series>, RegisterError> {
        if !valid_name(name) {
            return Err(RegisterError::Invalid(format!(
                "'{name}' is not a metric name (letters, digits, '_'; not starting with a digit)"
            )));
        }
        if let Some((k, _)) = labels
            .iter()
            .find(|(k, _)| !valid_name(k) || k.starts_with("__") || k == "app" || k == "le")
        {
            return Err(RegisterError::Invalid(format!(
                "'{k}' is not an allowed label name"
            )));
        }
        let mut sorted = labels.to_vec();
        sorted.sort();
        if sorted.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(RegisterError::Invalid("a label is given twice".into()));
        }
        let bounds: Vec<f64> = if buckets.is_empty() {
            DEFAULT_BUCKETS.to_vec()
        } else {
            buckets.to_vec()
        };
        if kind == Kind::Histogram
            && (bounds.iter().any(|b| !b.is_finite()) || bounds.windows(2).any(|w| w[0] >= w[1]))
        {
            return Err(RegisterError::Invalid(
                "buckets must be finite and increasing".into(),
            ));
        }
        let mut families = self.families.lock().unwrap_or_else(PoisonError::into_inner);
        let held: usize = families.values().map(|f| f.series.len()).sum();
        let family = families.entry(name.to_string()).or_insert_with(|| Family {
            kind,
            help: help.to_string(),
            series: BTreeMap::new(),
        });
        if family.kind != kind {
            return Err(RegisterError::Invalid(format!(
                "'{name}' is already a {}",
                family.kind.name()
            )));
        }
        if let Some(s) = family.series.get(&sorted) {
            return Ok(s.clone());
        }
        if held >= max_series {
            if family.series.is_empty() {
                families.remove(name);
            }
            return Err(RegisterError::LimitReached);
        }
        let series = Arc::new(Series {
            kind,
            value: AtomicF64::default(),
            histogram: (kind == Kind::Histogram).then(|| Histogram::new(&bounds)),
        });
        family.series.insert(sorted, series.clone());
        Ok(series)
    }

    fn write(&self, out: &mut String, app: &str) {
        let families = self.families.lock().unwrap_or_else(PoisonError::into_inner);
        for (name, family) in families.iter() {
            let _ = writeln!(out, "# HELP {name} {}", escape_help(&family.help));
            let _ = writeln!(out, "# TYPE {name} {}", family.kind.name());
            for (labels, series) in &family.series {
                let mut text = format!("app=\"{}\"", escape(app));
                for (k, v) in labels {
                    let _ = write!(text, ",{k}=\"{}\"", escape(v));
                }
                match &series.histogram {
                    Some(h) => h.write(out, name, &text),
                    None => {
                        let _ =
                            writeln!(out, "{name}{{{text}}} {}", format_f64(series.value.get()));
                    }
                }
            }
        }
    }
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn escape_help(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

/// The host's counters for one app: what B1's probes report, counted.
#[derive(Debug)]
pub(crate) struct AppStats {
    /// Callbacks run: call, cast, info.
    callbacks: [AtomicU64; 3],
    /// Guest time per callback.
    guest_time: Histogram,
    /// Messages dropped at a full mailbox by instances that have exited
    /// (live instances' counts are added when rendering).
    pub(crate) dropped: AtomicU64,
    /// Instance exits by reason class.
    exits: Mutex<BTreeMap<&'static str, u64>>,
}

impl Default for AppStats {
    fn default() -> Self {
        Self {
            callbacks: Default::default(),
            guest_time: Histogram::new(&CALLBACK_BUCKETS),
            dropped: AtomicU64::new(0),
            exits: Mutex::default(),
        }
    }
}

impl AppStats {
    /// A callback of `kind` ("call", "cast" or "info") ran for `guest`.
    pub(crate) fn callback(&self, kind: &str, guest: Duration) {
        let i = match kind {
            "call" => 0,
            "cast" => 1,
            _ => 2,
        };
        self.callbacks[i].fetch_add(1, Ordering::Relaxed);
        self.guest_time.record(guest.as_secs_f64());
    }

    /// An instance exited with `reason`.
    pub(crate) fn exited(&self, reason: &ExitReason) {
        let class = match reason {
            ExitReason::Normal => "normal",
            ExitReason::Shutdown => "shutdown",
            ExitReason::Killed => "killed",
            ExitReason::Noproc => "noproc",
            ExitReason::Noconnection => "noconnection",
            ExitReason::Timeout => "timeout",
            ExitReason::Crashed(_) => "crashed",
        };
        *self
            .exits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(class)
            .or_default() += 1;
    }
}

/// One app's metrics and host counters.
type AppEntry = (Arc<AppMetrics>, Arc<AppStats>);

/// Every app's metrics and stats on the node.
#[derive(Debug, Default)]
pub struct Metrics {
    apps: Mutex<HashMap<String, AppEntry>>,
}

impl Metrics {
    fn entry(&self, app: &str) -> AppEntry {
        self.apps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(app.to_string())
            .or_default()
            .clone()
    }

    /// `app`'s own metrics.
    pub(crate) fn app(&self, app: &str) -> Arc<AppMetrics> {
        self.entry(app).0
    }

    /// `app`'s host counters.
    pub(crate) fn stats(&self, app: &str) -> Arc<AppStats> {
        self.entry(app).1
    }

    /// Everything in the Prometheus text format (version 0.0.4): host
    /// metrics for every app (with `instances`' live gauges), then each
    /// app's own.
    #[must_use]
    pub fn render(&self, instances: &[InstanceHandle]) -> String {
        let apps: BTreeMap<String, AppEntry> = self
            .apps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut live: BTreeMap<&str, (u64, u64, u64)> = BTreeMap::new();
        let mut live_dropped: BTreeMap<&str, u64> = BTreeMap::new();
        for h in instances {
            let e = live.entry(h.app()).or_default();
            e.0 += 1;
            e.1 += h.memory_bytes();
            e.2 += u64::try_from(h.queued()).unwrap_or(u64::MAX);
            *live_dropped.entry(h.app()).or_default() += h.dropped_messages();
        }
        let mut out = String::new();
        let header = |out: &mut String, name: &str, kind: &str, help: &str| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} {kind}");
        };
        header(&mut out, "paddock_instances", "gauge", "Live instances.");
        for (app, (n, _, _)) in &live {
            let _ = writeln!(out, "paddock_instances{{app=\"{}\"}} {n}", escape(app));
        }
        header(
            &mut out,
            "paddock_memory_bytes",
            "gauge",
            "Linear memory of live instances.",
        );
        for (app, (_, m, _)) in &live {
            let _ = writeln!(out, "paddock_memory_bytes{{app=\"{}\"}} {m}", escape(app));
        }
        header(
            &mut out,
            "paddock_mailbox_depth",
            "gauge",
            "Messages waiting in live instances' mailboxes.",
        );
        for (app, (_, _, q)) in &live {
            let _ = writeln!(out, "paddock_mailbox_depth{{app=\"{}\"}} {q}", escape(app));
        }
        header(
            &mut out,
            "paddock_callbacks_total",
            "counter",
            "Callbacks run, by kind.",
        );
        for (app, (_, s)) in &apps {
            for (i, kind) in ["call", "cast", "info"].iter().enumerate() {
                let n = s.callbacks[i].load(Ordering::Relaxed);
                let _ = writeln!(
                    out,
                    "paddock_callbacks_total{{app=\"{}\",kind=\"{kind}\"}} {n}",
                    escape(app)
                );
            }
        }
        header(
            &mut out,
            "paddock_callback_guest_seconds",
            "histogram",
            "Guest execution time per callback.",
        );
        for (app, (_, s)) in &apps {
            s.guest_time.write(
                &mut out,
                "paddock_callback_guest_seconds",
                &format!("app=\"{}\"", escape(app)),
            );
        }
        header(
            &mut out,
            "paddock_mailbox_dropped_total",
            "counter",
            "Messages dropped at a full mailbox.",
        );
        for (app, (_, s)) in &apps {
            let _ = writeln!(
                out,
                "paddock_mailbox_dropped_total{{app=\"{}\"}} {}",
                escape(app),
                s.dropped.load(Ordering::Relaxed)
                    + live_dropped.get(app.as_str()).copied().unwrap_or(0)
            );
        }
        header(
            &mut out,
            "paddock_instance_exits_total",
            "counter",
            "Instance exits, by reason.",
        );
        for (app, (_, s)) in &apps {
            let exits = s
                .exits
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            for (reason, n) in exits {
                let _ = writeln!(
                    out,
                    "paddock_instance_exits_total{{app=\"{}\",reason=\"{reason}\"}} {n}",
                    escape(app)
                );
            }
        }
        for (app, (m, _)) in &apps {
            m.write(&mut out, app);
        }
        out
    }
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod metrics_tests;
