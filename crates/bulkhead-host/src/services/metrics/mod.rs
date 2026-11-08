//! `bulkhead:metrics`: app-defined counters, gauges and histograms
//! ([`crate::metrics`] keeps and renders them).
//!
//! An instance registers a series once and updates it by id, so the hot
//! path is one host call with no strings. Ids index the instance's own list,
//! and registering a series it already holds returns its id, so the list
//! is bounded by the app's `max_series`.

use std::collections::HashMap;
use std::sync::Arc;

use bulkhead_bundle::manifest::{AppSpec, Capability};
use bulkhead_native::protocol::{MetricError, MetricKind, MetricUpdate};

use super::{InstanceInfo, Service};
use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::metrics::{AppMetrics, Kind, Refused, RegisterError, Series};

/// Series an app may hold when its manifest sets no `max_series`.
pub const DEFAULT_MAX_SERIES: u32 = 1_000;

/// The metrics service.
pub(crate) struct MetricsService;

/// What the manifest asks of metrics for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct MetricsSpec {
    /// The prefix of every name the app exports.
    pub prefix: String,
    /// Most series the app may hold.
    pub max_series: u32,
}

/// Metrics state for one instance.
pub(crate) struct Instance {
    app: Arc<AppMetrics>,
    spec: MetricsSpec,
    granted: bool,
    held: Vec<Arc<Series>>,
    /// Ids by the address of the series they name (kept alive by `held`).
    ids: HashMap<usize, u32>,
}

impl Service for MetricsService {
    const CAPABILITY: Capability = Capability::Metrics;
    type Spec = MetricsSpec;
    type Shared = ();
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> MetricsSpec {
        let scope = manifest.capabilities.metrics.as_ref();
        MetricsSpec {
            prefix: scope.map(|s| s.prefix.clone()).unwrap_or_default(),
            max_series: scope
                .and_then(|s| s.max_series)
                .unwrap_or(DEFAULT_MAX_SERIES),
        }
    }

    fn shared(_config: &RuntimeConfig) -> Result<(), HostError> {
        Ok(())
    }

    fn instance(_shared: &Arc<()>, spec: &MetricsSpec, info: &InstanceInfo<'_>) -> Instance {
        Instance {
            app: info.metrics.app(info.handle.app()),
            spec: spec.clone(),
            granted: info.granted,
            held: Vec::new(),
            ids: HashMap::new(),
        }
    }
}

fn refused(r: Refused) -> MetricError {
    match r {
        Refused::Invalid(why) => MetricError::Invalid(why),
    }
}

/// The metrics functions on the host's own types: what a component's
/// `bulkhead:metrics` import and a native app's requests both run (ROADMAP
/// Z9b).
impl Instance {
    fn series(&self, id: u32) -> std::result::Result<&Arc<Series>, MetricError> {
        if !self.granted {
            return Err(MetricError::AccessDenied);
        }
        usize::try_from(id)
            .ok()
            .and_then(|i| self.held.get(i))
            .ok_or(MetricError::UnknownMetric)
    }

    /// Register a metric, or find the one registered with the same name
    /// and labels: its id.
    pub(crate) fn register_metric(
        &mut self,
        kind: Kind,
        name: &str,
        help: &str,
        labels: &[(String, String)],
        buckets: &[f64],
    ) -> std::result::Result<u32, MetricError> {
        if !self.granted {
            return Err(MetricError::AccessDenied);
        }
        let full = format!("{}_{name}", self.spec.prefix);
        let max = usize::try_from(self.spec.max_series).unwrap_or(usize::MAX);
        let series = match self.app.register(kind, &full, help, labels, buckets, max) {
            Ok(s) => s,
            Err(RegisterError::Invalid(why)) => return Err(MetricError::Invalid(why)),
            Err(RegisterError::LimitReached) => return Err(MetricError::LimitReached),
        };
        let key = Arc::as_ptr(&series).addr();
        if let Some(id) = self.ids.get(&key) {
            return Ok(*id);
        }
        let Ok(id) = u32::try_from(self.held.len()) else {
            return Err(MetricError::LimitReached);
        };
        self.held.push(series);
        self.ids.insert(key, id);
        Ok(id)
    }

    /// Add `delta` to a counter or gauge.
    pub(crate) fn metric_add(&self, id: u32, delta: f64) -> std::result::Result<(), MetricError> {
        self.series(id).and_then(|s| s.add(delta).map_err(refused))
    }

    /// Set a gauge.
    pub(crate) fn metric_set(&self, id: u32, value: f64) -> std::result::Result<(), MetricError> {
        self.series(id).and_then(|s| s.set(value).map_err(refused))
    }

    /// Record a histogram observation.
    pub(crate) fn metric_observe(
        &self,
        id: u32,
        value: f64,
    ) -> std::result::Result<(), MetricError> {
        self.series(id)
            .and_then(|s| s.record(value).map_err(refused))
    }

    /// Register a metric for a native app.
    pub(crate) fn native_register(
        &mut self,
        kind: MetricKind,
        name: &str,
        help: &str,
        labels: &[(String, String)],
        buckets: &[f64],
    ) -> std::result::Result<u32, MetricError> {
        let kind = match kind {
            MetricKind::Counter => Kind::Counter,
            MetricKind::Gauge => Kind::Gauge,
            MetricKind::Histogram => Kind::Histogram,
        };
        self.register_metric(kind, name, help, labels, buckets)
    }

    /// Apply a native app's batch of updates; the first it refuses, if
    /// any (the rest are applied).
    pub(crate) fn native_updates(&self, updates: &[MetricUpdate]) -> Option<MetricError> {
        let mut first = None;
        for update in updates {
            let outcome = match *update {
                MetricUpdate::Add(id, delta) => self.metric_add(id, delta),
                MetricUpdate::Set(id, value) => self.metric_set(id, value),
                MetricUpdate::Observe(id, value) => self.metric_observe(id, value),
            };
            if let Err(e) = outcome {
                first.get_or_insert(e);
            }
        }
        first
    }
}
