//! Per-app rate limits for guest log records.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

/// Whether a record may be emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Emit it; `dropped` records were dropped since the last one emitted.
    Emit { dropped: u64 },
    /// Drop it.
    Drop,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
    dropped: u64,
}

/// A token bucket per app: `rate` records per second, with bursts up to one
/// second's worth. Shared by every instance of the app, so a pool cannot
/// multiply its allowance.
#[derive(Debug, Default)]
pub(crate) struct LogLimits {
    apps: Mutex<HashMap<String, Bucket>>,
}

impl LogLimits {
    pub(crate) fn admit(&self, app: &str, rate: Option<u32>) -> Admit {
        self.admit_at(app, rate, Instant::now())
    }

    pub(crate) fn admit_at(&self, app: &str, rate: Option<u32>, now: Instant) -> Admit {
        let Some(rate) = rate.filter(|r| *r > 0) else {
            return Admit::Emit { dropped: 0 };
        };
        let rate = f64::from(rate);
        let mut apps = self.apps.lock().unwrap_or_else(PoisonError::into_inner);
        let bucket = apps.entry(app.to_string()).or_insert(Bucket {
            tokens: rate,
            last: now,
            dropped: 0,
        });
        let refill = now.saturating_duration_since(bucket.last).as_secs_f64() * rate;
        bucket.tokens = (bucket.tokens + refill).min(rate);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Admit::Emit {
                dropped: std::mem::take(&mut bucket.dropped),
            }
        } else {
            bucket.dropped += 1;
            Admit::Drop
        }
    }
}

#[cfg(test)]
#[path = "limits_tests.rs"]
mod limits_tests;
