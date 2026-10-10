//! App-defined metrics through `paddock:metrics` (cargo feature `metrics`).
//!
//! Declare `capabilities."paddock:metrics"` with a `prefix`; the node
//! exports every metric as `<prefix>_<name>` with an `app` label, in the
//! Prometheus text format, beside its own. Metrics are per app: all of an
//! app's instances update the same series. Create metrics in `init` and
//! keep them in state; updating one is a single host call.
//!
//! ```ignore
//! use paddock_sdk::metrics::{Counter, Histogram};
//!
//! let orders = Counter::new("orders_total", "Orders taken.", &[("region", "eu")])?;
//! let latency = Histogram::new("handle_seconds", "Time per order.", &[], &[])?;
//! orders.inc();
//! latency.observe(0.003);
//! ```

use std::fmt;

use crate::bindings::paddock::metrics::metrics as wit;

/// Why a metric could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetricsError {
    /// `paddock:metrics` is not granted.
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

impl From<wit::Error> for MetricsError {
    fn from(e: wit::Error) -> Self {
        match e {
            wit::Error::AccessDenied => Self::AccessDenied,
            wit::Error::Invalid(why) => Self::Invalid(why),
            wit::Error::LimitReached => Self::LimitReached,
            wit::Error::UnknownMetric => Self::Invalid("unknown metric".into()),
        }
    }
}

fn register(
    kind: wit::Kind,
    name: &str,
    help: &str,
    labels: &[(&str, &str)],
    buckets: &[f64],
) -> Result<u32, MetricsError> {
    let labels: Vec<(String, String)> = labels
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    Ok(wit::register(kind, name, help, &labels, buckets)?)
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
        register(wit::Kind::Counter, name, help, labels, &[]).map(Self)
    }

    /// Add one.
    pub fn inc(self) {
        self.add(1.0);
    }

    /// Add `n` (ignored if negative or not finite).
    pub fn add(self, n: f64) {
        let _ = wit::add(self.0, n);
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
        register(wit::Kind::Gauge, name, help, labels, &[]).map(Self)
    }

    /// Set it (ignored if not finite).
    pub fn set(self, value: f64) {
        let _ = wit::set(self.0, value);
    }

    /// Move it by `delta` (ignored if not finite).
    pub fn add(self, delta: f64) {
        let _ = wit::add(self.0, delta);
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
        register(wit::Kind::Histogram, name, help, labels, buckets).map(Self)
    }

    /// Record `value` (ignored if not finite).
    pub fn observe(self, value: f64) {
        let _ = wit::observe(self.0, value);
    }
}
