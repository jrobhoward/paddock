//! `wasi:logging`: guest log records as `tracing` events on the
//! `paddock::guest` target, tagged with app, version and pid.
//!
//! Records below the app's level are dropped; the operator can override the
//! level per app at runtime ([`Runtime::set_log_level`](crate::Runtime::set_log_level)).
//! A token bucket per app, shared by its instances, enforces
//! `rate_limit_per_sec` and reports how many records it dropped. Without the
//! capability granted, records are dropped silently.
//!
//! The service announces the effective level to each instance as
//! `host-event("log-level", "<level>")`, after `init`, after a snapshot
//! upgrade and on every operator change, so a guest can skip building
//! records that would be dropped.

mod limits;

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use paddock_bundle::manifest::{AppSpec, Capability, LogLevel};
use wasmtime::Result;
use wasmtime::component::Linker;

use super::{Ctx, HostState, InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;
use crate::message::Envelope;
use crate::output::{Tags, escape_control};
use limits::{Admit, LogLimits};

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
        world: "paddock:host/logging",
        imports: { default: async | trappable },
    });
}

use bindings::wasi::logging::logging;

/// The logging service.
pub(crate) struct Logging;

/// What the manifest asks of logging for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct LoggingSpec {
    /// Records below this level are dropped.
    pub level: LogLevel,
    /// Most records per second for the whole app; `None` for no limit.
    pub rate_limit: Option<u32>,
}

/// Node-wide logging state.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    limits: LogLimits,
    overrides: RwLock<HashMap<String, LogLevel>>,
    /// The node's ceiling on any app's records per second; `None` for none.
    node_rate: Option<u32>,
}

/// One app's log budget: its `rate_limit_per_sec`, under the node's
/// ceiling, shared by its `wasi:logging` records and captured output.
#[derive(Debug, Clone)]
pub(crate) struct LogRate {
    shared: Arc<Shared>,
    app_rate: Option<u32>,
}

impl LogRate {
    pub(crate) fn new(shared: Arc<Shared>, app_rate: Option<u32>) -> Self {
        Self { shared, app_rate }
    }

    /// `Some(dropped)` if a record may be emitted now, with how many were
    /// dropped since the last one; `None` to drop it.
    pub(crate) fn admit(&self, app: &str) -> Option<u64> {
        match self
            .shared
            .limits
            .admit(app, self.shared.rate(self.app_rate))
        {
            Admit::Emit { dropped } => Some(dropped),
            Admit::Drop => None,
        }
    }
}

impl Shared {
    /// An app's effective rate: the lower of its own and the node's.
    pub(crate) fn rate(&self, app_rate: Option<u32>) -> Option<u32> {
        let app_rate = app_rate.filter(|r| *r > 0);
        match (app_rate, self.node_rate) {
            (Some(a), Some(n)) => Some(a.min(n)),
            (a, n) => a.or(n),
        }
    }

    /// Override `app`'s level, or remove the override with `None`.
    pub(crate) fn set_override(&self, app: &str, level: Option<LogLevel>) {
        let mut levels = self
            .overrides
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        match level {
            Some(level) => levels.insert(app.to_string(), level),
            None => levels.remove(app),
        };
    }

    /// The operator's level for `app`, if one is set.
    pub(crate) fn override_for(&self, app: &str) -> Option<LogLevel> {
        self.overrides
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(app)
            .copied()
    }
}

/// Logging state for one instance.
pub(crate) struct Instance {
    shared: Arc<Shared>,
    spec: LoggingSpec,
    granted: bool,
}

impl Service for Logging {
    const CAPABILITY: Capability = Capability::Logging;
    const INTERFACES: &'static [&'static str] = &["wasi:logging/logging"];
    type Spec = LoggingSpec;
    type Shared = Shared;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> LoggingSpec {
        let scope = manifest.capabilities.logging.as_ref();
        LoggingSpec {
            level: scope.map_or(manifest.observability.log_level, |l| l.max_level),
            rate_limit: scope.and_then(|l| l.rate_limit_per_sec),
        }
    }

    fn shared(config: &RuntimeConfig) -> Result<Shared, HostError> {
        Ok(Shared {
            node_rate: Some(config.log_records_per_sec).filter(|r| *r > 0),
            ..Shared::default()
        })
    }

    fn instance(shared: &Arc<Shared>, spec: &LoggingSpec, info: &InstanceInfo<'_>) -> Instance {
        Instance {
            shared: shared.clone(),
            spec: spec.clone(),
            granted: info.granted,
        }
    }

    fn link<T: HostState<Self>>(linker: &mut Linker<T>) -> Result<()> {
        logging::add_to_linker::<T, Ctx<Self>>(linker, T::service)
    }

    /// `log-level` with the effective level (`trace` to `critical`), so the
    /// guest SDK can skip formatting records the host would drop. Only to
    /// instances granted logging; the host still filters every record.
    fn announce(state: &Instance, tags: &Tags) -> Vec<(String, Envelope)> {
        if !state.granted {
            return Vec::new();
        }
        let level = state
            .shared
            .override_for(&tags.app)
            .unwrap_or(state.spec.level);
        vec![(
            LEVEL_EVENT.to_string(),
            Envelope::new("text/plain", level_name(level)),
        )]
    }
}

/// The host event that carries the effective log level.
pub const LEVEL_EVENT: &str = "log-level";

fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "trace",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
        LogLevel::Critical => "critical",
    }
}

fn level_of(level: logging::Level) -> LogLevel {
    match level {
        logging::Level::Trace => LogLevel::Trace,
        logging::Level::Debug => LogLevel::Debug,
        logging::Level::Info => LogLevel::Info,
        logging::Level::Warn => LogLevel::Warn,
        logging::Level::Error => LogLevel::Error,
        logging::Level::Critical => LogLevel::Critical,
    }
}

impl logging::Host for ServiceCtx<'_, Instance> {
    async fn log(&mut self, level: logging::Level, context: String, message: String) -> Result<()> {
        let me = &*self.state;
        if !me.granted {
            return Ok(());
        }
        let level = level_of(level);
        let t = self.tags;
        let cap = me.shared.override_for(&t.app).unwrap_or(me.spec.level);
        if level < cap {
            return Ok(());
        }
        match me
            .shared
            .limits
            .admit(&t.app, me.shared.rate(me.spec.rate_limit))
        {
            Admit::Drop => return Ok(()),
            Admit::Emit { dropped: 0 } => {}
            Admit::Emit { dropped } => {
                tracing::warn!(target: "paddock::guest", app = %t.app, version = %t.version, pid = %t.pid, dropped, "log records dropped by the rate limit");
            }
        }
        let (context, message) = (escape_control(&context), escape_control(&message));
        let context = context.as_ref();
        let pid = t.pid.to_string();
        let (app, version) = (t.app.as_str(), t.version.as_str());
        match level {
            LogLevel::Trace => {
                tracing::trace!(target: "paddock::guest", app, version, pid, context, "{message}")
            }
            LogLevel::Debug => {
                tracing::debug!(target: "paddock::guest", app, version, pid, context, "{message}")
            }
            LogLevel::Info => {
                tracing::info!(target: "paddock::guest", app, version, pid, context, "{message}")
            }
            LogLevel::Warn => {
                tracing::warn!(target: "paddock::guest", app, version, pid, context, "{message}")
            }
            LogLevel::Error => {
                tracing::error!(target: "paddock::guest", app, version, pid, context, "{message}")
            }
            LogLevel::Critical => {
                tracing::error!(target: "paddock::guest", app, version, pid, context, critical = true, "{message}")
            }
        }
        Ok(())
    }
}

/// A native app's log record (ROADMAP N2), through the same level and rate
/// checks as a component's `wasi:logging` records.
pub(crate) async fn record(
    ctx: &mut ServiceCtx<'_, Instance>,
    level: paddock_native::protocol::Level,
    message: String,
) {
    use paddock_native::protocol::Level;
    let level = match level {
        Level::Trace => logging::Level::Trace,
        Level::Debug => logging::Level::Debug,
        Level::Info => logging::Level::Info,
        Level::Warn => logging::Level::Warn,
        Level::Error => logging::Level::Error,
        Level::Critical => logging::Level::Critical,
    };
    let _ = logging::Host::log(ctx, level, String::new(), message).await;
}

#[cfg(test)]
#[path = "logging_tests.rs"]
mod logging_tests;
