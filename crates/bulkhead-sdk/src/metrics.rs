//! App-defined metrics through `bulkhead:metrics` (cargo feature `metrics`).
//!
//! Declare `capabilities."bulkhead:metrics"` with a `prefix`; the node
//! exports every metric as `<prefix>_<name>` with an `app` label, in the
//! Prometheus text format, beside its own. Metrics are per app: all of an
//! app's instances update the same series. Create metrics in `init` and
//! keep them in state; updating one is a single host call.
//!
//! ```ignore
//! use bulkhead_sdk::metrics::{Counter, Histogram};
//!
//! let orders = Counter::new("orders_total", "Orders taken.", &[("region", "eu")])?;
//! let latency = Histogram::new("handle_seconds", "Time per order.", &[], &[])?;
//! orders.inc();
//! latency.observe(0.003);
//! ```

use std::fmt;

use bulkhead_native::app::Ctx;
use bulkhead_native::protocol as p;

/// Why a metric could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetricsError {
    /// `bulkhead:metrics` is not granted.
    AccessDenied,
    /// A bad name, label or buckets, or a name used with another kind.
    Invalid(String),
    /// The app holds its `max_series` series already.
    LimitReached,
}

impl fmt::Display for MetricsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("access denied"),
            Self::Invalid(why) => write!(f, "invalid: {why}"),
            Self::LimitReached => f.write_str("series limit reached"),
        }
    }
}

impl std::error::Error for MetricsError {}

impl From<p::MetricError> for MetricsError {
    fn from(e: p::MetricError) -> Self {
        match e {
            p::MetricError::AccessDenied => Self::AccessDenied,
            p::MetricError::Invalid(why) => Self::Invalid(why),
            p::MetricError::LimitReached => Self::LimitReached,
            p::MetricError::UnknownMetric => Self::Invalid("unknown metric".into()),
        }
    }
}

fn register(
    kind: p::MetricKind,
    name: &str,
    help: &str,
    labels: &[(&str, &str)],
    buckets: &[f64],
) -> Result<u32, MetricsError> {
    Ok(Ctx::current().register_metric(kind, name, help, labels, buckets)?)
}

/// A counter: only goes up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counter(u32);

impl Counter {
    /// The counter `name` with `labels`.
    ///
    /// # Errors
    ///
    /// As [`MetricsError`] lists.
    pub fn new(name: &str, help: &str, labels: &[(&str, &str)]) -> Result<Self, MetricsError> {
        register(p::MetricKind::Counter, name, help, labels, &[]).map(Self)
    }

    /// Add one.
    pub fn inc(self) {
        self.add(1.0);
    }

    /// Add `n` (ignored if negative or not finite).
    pub fn add(self, n: f64) {
        Ctx::current().metric_add(self.0, n);
    }
}

/// A gauge: set, or moved either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gauge(u32);

impl Gauge {
    /// The gauge `name` with `labels`.
    ///
    /// # Errors
    ///
    /// As [`MetricsError`] lists.
    pub fn new(name: &str, help: &str, labels: &[(&str, &str)]) -> Result<Self, MetricsError> {
        register(p::MetricKind::Gauge, name, help, labels, &[]).map(Self)
    }

    /// Set it (ignored if not finite).
    pub fn set(self, value: f64) {
        Ctx::current().metric_set(self.0, value);
    }

    /// Move it by `delta` (ignored if not finite).
    pub fn add(self, delta: f64) {
        Ctx::current().metric_add(self.0, delta);
    }
}

/// A histogram: observations counted into buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Histogram(u32);

impl Histogram {
    /// The histogram `name` with `labels` and `buckets` (upper bounds,
    /// increasing; empty for 0.005 to 10 seconds).
    ///
    /// # Errors
    ///
    /// As [`MetricsError`] lists.
    pub fn new(
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
        buckets: &[f64],
    ) -> Result<Self, MetricsError> {
        register(p::MetricKind::Histogram, name, help, labels, buckets).map(Self)
    }

    /// Record `value` (ignored if not finite).
    pub fn observe(self, value: f64) {
        Ctx::current().metric_observe(self.0, value);
    }
}
