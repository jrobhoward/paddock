//! Health checks (ROADMAP D7): an app whose manifest has a `health` section
//! is called every `interval_ms` with a [`HEALTH_SCHEMA`] envelope, on each
//! instance, from the moment `init` returns. A reply whose body is `ok` is
//! healthy; any other reply, an error, or no reply within the interval is a
//! failure. After `failure_threshold` failures in a row the instance is
//! killed with `crashed: unhealthy`, and its supervisor restarts it.
//!
//! The check is an ordinary call, as an HTTP request is, so the frozen app
//! world gains nothing: `paddock-sdk` answers it from `App::health`. It goes
//! through the mailbox, so an instance that has stopped taking messages
//! fails it as well as one that says it is unwell.

use std::sync::Weak;
use std::time::Duration;

use crate::delivery;
use crate::instance::InstanceHandle;
use crate::message::{CallError, Envelope, ExitReason};
use crate::runtime::RuntimeInner;

/// The schema of a health check call and of its reply.
pub const HEALTH_SCHEMA: &str = "paddock/health";

/// The body of a healthy reply.
pub const HEALTHY: &[u8] = b"ok";

/// How often to check an instance, and how many failures in a row kill it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthCheck {
    /// Time between checks, and how long a check may take.
    pub interval: Duration,
    /// Failures in a row before the instance is killed as `unhealthy`.
    pub failure_threshold: u32,
}

/// The exit reason of an instance killed by its health checks.
pub(crate) fn unhealthy() -> ExitReason {
    ExitReason::Crashed("unhealthy".to_string())
}

/// What a check's answer means: `Ok` if healthy, else why not.
pub(crate) fn outcome(result: &Result<Envelope, CallError>) -> Result<(), String> {
    match result {
        Ok(reply) if reply.body == HEALTHY => Ok(()),
        Ok(reply) => Err(String::from_utf8_lossy(&reply.body).into_owned()),
        Err(e) => Err(e.to_string()),
    }
}

/// Failures in a row.
#[derive(Debug)]
pub(crate) struct Streak {
    threshold: u32,
    count: u32,
}

impl Streak {
    pub(crate) fn new(threshold: u32) -> Self {
        Self {
            threshold: threshold.max(1),
            count: 0,
        }
    }

    /// Record one check; returns whether the instance must be killed.
    pub(crate) fn record(&mut self, healthy: bool) -> bool {
        if healthy {
            self.count = 0;
            return false;
        }
        self.count += 1;
        self.count >= self.threshold
    }
}

/// Check `me` every interval until it exits or the runtime is gone, and
/// kill it once the checks have failed often enough in a row.
pub(crate) async fn watch(rt: Weak<RuntimeInner>, me: InstanceHandle, check: HealthCheck) {
    let mut streak = Streak::new(check.failure_threshold);
    loop {
        tokio::select! {
            _ = me.wait() => return,
            () = tokio::time::sleep(check.interval) => {}
        }
        let Some(rt) = rt.upgrade() else {
            return;
        };
        let msg = Envelope {
            schema: Some(HEALTH_SCHEMA.to_string()),
            ..Envelope::new("text/plain", "health")
        };
        let result = delivery::call(&rt, None, me.pid(), msg, check.interval).await;
        drop(rt);
        let answer = outcome(&result);
        if let Err(why) = &answer {
            tracing::warn!(pid = %me.pid(), app = me.app(), why, "health check failed");
        }
        if streak.record(answer.is_ok()) {
            tracing::warn!(
                pid = %me.pid(),
                app = me.app(),
                failures = check.failure_threshold,
                "health checks failed; killing the instance"
            );
            me.kill_with(unhealthy());
            return;
        }
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod health_tests;
