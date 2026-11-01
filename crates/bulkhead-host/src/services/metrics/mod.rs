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
use wasmtime::Result;
use wasmtime::component::Linker;

use super::{Ctx, HostState, InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::metrics::{AppMetrics, Kind, Refused, RegisterError, Series};

mod bindings {
    // Generated code carries no doc comments.
    #![allow(missing_docs)]

    wasmtime::component::bindgen!({
        path: [
            "../../wit",
            "../../wit/vendor/wasi-logging",
            "../../wit/vendor/wasi-config",
            "../../wit/vendor/wasi-keyvalue",
            "../../wit/groups",
            "../../wit/watch",
            "../../wit/secrets",
            "../../wit/metrics",
            "../../wit/sql",
            "wit",
        ],
        world: "bulkhead:host/metrics",
        imports: { default: async | trappable },
    });
}

use bindings::bulkhead::metrics::metrics as wit;

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
    const INTERFACES: &'static [&'static str] = &["bulkhead:metrics/metrics"];
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

    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> Result<()> {
        wit::add_to_linker::<T, Ctx<Self>>(linker, T::service)
    }
}

fn refused(r: Refused) -> wit::Error {
    match r {
        Refused::Invalid(why) => wit::Error::Invalid(why),
    }
}

impl Instance {
    fn series(&self, id: u32) -> std::result::Result<&Arc<Series>, wit::Error> {
        if !self.granted {
            return Err(wit::Error::AccessDenied);
        }
        usize::try_from(id)
            .ok()
            .and_then(|i| self.held.get(i))
            .ok_or(wit::Error::UnknownMetric)
    }
}

impl wit::Host for ServiceCtx<'_, Instance> {
    async fn register(
        &mut self,
        kind: wit::Kind,
        name: String,
        help: String,
        labels: Vec<(String, String)>,
        buckets: Vec<f64>,
    ) -> Result<std::result::Result<u32, wit::Error>> {
        let state = &mut *self.state;
        if !state.granted {
            return Ok(Err(wit::Error::AccessDenied));
        }
        let kind = match kind {
            wit::Kind::Counter => Kind::Counter,
            wit::Kind::Gauge => Kind::Gauge,
            wit::Kind::Histogram => Kind::Histogram,
        };
        let full = format!("{}_{name}", state.spec.prefix);
        let max = usize::try_from(state.spec.max_series).unwrap_or(usize::MAX);
        let series = match state
            .app
            .register(kind, &full, &help, &labels, &buckets, max)
        {
            Ok(s) => s,
            Err(RegisterError::Invalid(why)) => return Ok(Err(wit::Error::Invalid(why))),
            Err(RegisterError::LimitReached) => return Ok(Err(wit::Error::LimitReached)),
        };
        let key = Arc::as_ptr(&series).addr();
        if let Some(id) = state.ids.get(&key) {
            return Ok(Ok(*id));
        }
        let Ok(id) = u32::try_from(state.held.len()) else {
            return Ok(Err(wit::Error::LimitReached));
        };
        state.held.push(series);
        state.ids.insert(key, id);
        Ok(Ok(id))
    }

    async fn add(
        &mut self,
        metric: u32,
        delta: f64,
    ) -> Result<std::result::Result<(), wit::Error>> {
        Ok(self
            .state
            .series(metric)
            .and_then(|s| s.add(delta).map_err(refused)))
    }

    async fn set(
        &mut self,
        metric: u32,
        value: f64,
    ) -> Result<std::result::Result<(), wit::Error>> {
        Ok(self
            .state
            .series(metric)
            .and_then(|s| s.set(value).map_err(refused)))
    }

    async fn observe(
        &mut self,
        metric: u32,
        value: f64,
    ) -> Result<std::result::Result<(), wit::Error>> {
        Ok(self
            .state
            .series(metric)
            .and_then(|s| s.record(value).map_err(refused)))
    }
}

fn native_error(e: wit::Error) -> bulkhead_native::protocol::MetricError {
    use bulkhead_native::protocol::MetricError;
    match e {
        wit::Error::AccessDenied => MetricError::AccessDenied,
        wit::Error::Invalid(why) => MetricError::Invalid(why),
        wit::Error::LimitReached => MetricError::LimitReached,
        wit::Error::UnknownMetric => MetricError::UnknownMetric,
    }
}

impl ServiceCtx<'_, Instance> {
    /// Register a metric for a native app, as a component's
    /// `bulkhead:metrics` import does.
    pub(crate) async fn native_register(
        &mut self,
        kind: bulkhead_native::protocol::MetricKind,
        name: String,
        help: String,
        labels: Vec<(String, String)>,
        buckets: Vec<f64>,
    ) -> std::result::Result<u32, bulkhead_native::protocol::MetricError> {
        use bulkhead_native::protocol::{MetricError, MetricKind};
        use wit::Host;
        let kind = match kind {
            MetricKind::Counter => wit::Kind::Counter,
            MetricKind::Gauge => wit::Kind::Gauge,
            MetricKind::Histogram => wit::Kind::Histogram,
        };
        match Host::register(self, kind, name, help, labels, buckets).await {
            Ok(r) => r.map_err(native_error),
            Err(e) => Err(MetricError::Invalid(e.to_string())),
        }
    }

    /// Apply a native app's batch of updates; the first it refuses, if
    /// any (the rest are applied).
    pub(crate) fn native_updates(
        &self,
        updates: &[bulkhead_native::protocol::MetricUpdate],
    ) -> Option<bulkhead_native::protocol::MetricError> {
        use bulkhead_native::protocol::MetricUpdate;
        let mut first = None;
        for update in updates {
            let outcome = match *update {
                MetricUpdate::Add(id, delta) => self
                    .state
                    .series(id)
                    .and_then(|s| s.add(delta).map_err(refused)),
                MetricUpdate::Set(id, value) => self
                    .state
                    .series(id)
                    .and_then(|s| s.set(value).map_err(refused)),
                MetricUpdate::Observe(id, value) => self
                    .state
                    .series(id)
                    .and_then(|s| s.record(value).map_err(refused)),
            };
            if let Err(e) = outcome {
                first.get_or_insert(native_error(e));
            }
        }
        first
    }
}
