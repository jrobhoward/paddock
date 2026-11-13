//! Asking the node from the app's own threads (ROADMAP AP1).
//!
//! Only the callback thread asks the node through [`Ctx`]. A thread of the
//! app's own (one reading Kafka or a websocket, say) asks through a
//! [`Sender`]: a channel of its own, opened from a callback with
//! [`Ctx::sender`] and handed to the thread, or opened from another sender
//! with [`Sender::try_clone`]. A sender's requests are answered at any
//! time, never behind the callback that is running, and act as the
//! instance: its pid, its messaging rules, its grants.
//!
//! ```no_run
//! # use bulkhead_sdk::{Ctx, Envelope, Pid};
//! # fn started(ctx: &mut Ctx, target: Pid) -> Result<(), bulkhead_sdk::sender::SenderError> {
//! let mut sender = ctx.sender()?;
//! std::thread::spawn(move || {
//!     loop {
//!         // ... read an event from somewhere ...
//!         sender.cast(target, Envelope::text("event"));
//!     }
//! });
//! # Ok(())
//! # }
//! ```
//!
//! A sender serves one thread at a time (it is `Send`, not `Sync`); give
//! each thread its own. An app holds up to its `limits.senders_max` at
//! once (8 unless the node or the manifest says otherwise); dropping one
//! frees its place. A sender may cast, call (several calls in flight with
//! [`Sender::start_call`]), use buckets ([`Sender::bucket`]) and record
//! metrics; watches, links, monitors, timers and the registry stay the
//! callback thread's.

use std::fmt;
use std::time::Duration;

use bulkhead_native::protocol as p;
use bulkhead_native::sender::Sender as Channel;

use crate::ctx::Ctx;
use crate::envelope::Envelope;
use crate::types::{CallError, CastError, Pid};

/// Why no sender was made.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SenderError {
    /// The app holds its `limits.senders_max` senders already.
    LimitReached,
    /// The instance cannot have senders (an in-process test app).
    Unsupported,
    /// The channel could not be made.
    Other(String),
}

impl fmt::Display for SenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitReached => f.write_str("senders_max reached"),
            Self::Unsupported => f.write_str("this instance cannot have senders"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for SenderError {}

impl From<p::SenderError> for SenderError {
    fn from(e: p::SenderError) -> Self {
        match e {
            p::SenderError::LimitReached => Self::LimitReached,
            p::SenderError::Unsupported => Self::Unsupported,
            p::SenderError::Other(why) => Self::Other(why),
        }
    }
}

impl Ctx {
    /// A sender for one of the app's threads.
    ///
    /// # Errors
    ///
    /// [`SenderError::LimitReached`] at the app's `limits.senders_max`,
    /// or why the channel could not be made.
    pub fn sender(&mut self) -> Result<Sender, SenderError> {
        Ok(Sender(bulkhead_native::app::Ctx::current().sender()?))
    }
}

/// One thread's channel to the node.
#[derive(Debug)]
pub struct Sender(Channel);

/// A timeout in milliseconds as the protocol has it.
fn ms(timeout: Duration) -> u32 {
    u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX)
}

impl Sender {
    /// Another sender, for another thread.
    ///
    /// # Errors
    ///
    /// As [`Ctx::sender`].
    pub fn try_clone(&mut self) -> Result<Self, SenderError> {
        Ok(Self(self.0.try_clone()?))
    }

    /// This instance's pid.
    pub fn pid(&mut self) -> Pid {
        self.0.self_pid().into()
    }

    /// Send without waiting, as [`Ctx::cast`].
    pub fn cast(&mut self, to: Pid, msg: Envelope) {
        self.0.cast(to.into(), msg.into());
    }

    /// [`Sender::cast`], saying whether the message was queued, as
    /// [`Ctx::try_cast`].
    ///
    /// # Errors
    ///
    /// [`CastError`] as [`Ctx::try_cast`] gives it.
    pub fn try_cast(&mut self, to: Pid, msg: Envelope) -> Result<(), CastError> {
        self.0.try_cast(to.into(), msg.into()).map_err(Into::into)
    }

    /// [`Sender::try_cast`] that waits up to `timeout` for room in a full
    /// mailbox. Only this thread waits; the callbacks go on.
    ///
    /// # Errors
    ///
    /// [`CastError`], [`CastError::Timeout`] if no room came in time.
    pub fn cast_timeout(
        &mut self,
        to: Pid,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<(), CastError> {
        self.0
            .cast_timeout(to.into(), msg.into(), ms(timeout))
            .map_err(Into::into)
    }

    /// Call `to` and wait up to `timeout` for its reply. Only this thread
    /// waits; a call to this instance's own pid is answered by its
    /// callbacks.
    ///
    /// # Errors
    ///
    /// The [`CallError`] if there is no reply.
    pub fn call(
        &mut self,
        to: Pid,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, CallError> {
        self.0
            .call(to.into(), msg.into(), ms(timeout))
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Start a call and return its id without waiting; take its reply with
    /// [`Sender::next_answer`]. Up to 64 calls in flight on one sender go
    /// at once; more wait their turn.
    pub fn start_call(&mut self, to: Pid, msg: Envelope, timeout: Duration) -> u64 {
        self.0.start_call(to.into(), msg.into(), ms(timeout))
    }

    /// Calls started and not yet taken by [`Sender::next_answer`].
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.0.in_flight()
    }

    /// The next started call's reply, in the order replies come, waiting
    /// for one: its id and its result. `None` when no call is in flight.
    pub fn next_answer(&mut self) -> Option<(u64, Result<Envelope, CallError>)> {
        self.0
            .next_answer()
            .map(|(id, result)| (id, result.map(Into::into).map_err(Into::into)))
    }

    /// The bucket the manifest lists as `name`, through this sender.
    #[cfg(feature = "keyvalue")]
    pub fn bucket<'a>(&'a mut self, name: &'a str) -> SenderBucket<'a> {
        SenderBucket { sender: self, name }
    }

    /// Register a counter for this sender: the same series as the
    /// callbacks' [`crate::metrics::Counter`] of that name and labels.
    ///
    /// # Errors
    ///
    /// As [`crate::metrics::MetricsError`] lists.
    #[cfg(feature = "metrics")]
    pub fn counter(
        &mut self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
    ) -> Result<SenderMetric, crate::metrics::MetricsError> {
        self.register(p::MetricKind::Counter, name, help, labels, &[])
    }

    /// Register a gauge for this sender, as [`Sender::counter`].
    ///
    /// # Errors
    ///
    /// As [`crate::metrics::MetricsError`] lists.
    #[cfg(feature = "metrics")]
    pub fn gauge(
        &mut self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
    ) -> Result<SenderMetric, crate::metrics::MetricsError> {
        self.register(p::MetricKind::Gauge, name, help, labels, &[])
    }

    /// Register a histogram for this sender, as [`Sender::counter`].
    ///
    /// # Errors
    ///
    /// As [`crate::metrics::MetricsError`] lists.
    #[cfg(feature = "metrics")]
    pub fn histogram(
        &mut self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
        buckets: &[f64],
    ) -> Result<SenderMetric, crate::metrics::MetricsError> {
        self.register(p::MetricKind::Histogram, name, help, labels, buckets)
    }

    #[cfg(feature = "metrics")]
    fn register(
        &mut self,
        kind: p::MetricKind,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
        buckets: &[f64],
    ) -> Result<SenderMetric, crate::metrics::MetricsError> {
        self.0
            .register_metric(kind, name, help, labels, buckets)
            .map(SenderMetric)
            .map_err(Into::into)
    }

    /// Add `delta` to a counter or gauge this sender registered. Sent at
    /// once, with no answer.
    #[cfg(feature = "metrics")]
    pub fn add(&mut self, metric: SenderMetric, delta: f64) {
        self.0.metric_add(metric.0, delta);
    }

    /// Set a gauge this sender registered.
    #[cfg(feature = "metrics")]
    pub fn set(&mut self, metric: SenderMetric, value: f64) {
        self.0.metric_set(metric.0, value);
    }

    /// Record a histogram observation for one this sender registered.
    #[cfg(feature = "metrics")]
    pub fn observe(&mut self, metric: SenderMetric, value: f64) {
        self.0.metric_observe(metric.0, value);
    }
}

/// A metric a [`Sender`] registered: its updates go through that sender.
#[cfg(feature = "metrics")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderMetric(u32);

/// A bucket through a [`Sender`]: as [`crate::kv::Bucket`], from the
/// sender's thread. Reads go to the node, also for a published bucket.
#[cfg(feature = "keyvalue")]
#[derive(Debug)]
pub struct SenderBucket<'a> {
    sender: &'a mut Sender,
    name: &'a str,
}

#[cfg(feature = "keyvalue")]
impl SenderBucket<'_> {
    fn op(
        &mut self,
        op: bulkhead_native::hot::KvOp,
        key: &str,
        value: &[u8],
    ) -> Result<p::KvAnswer, crate::kv::KvError> {
        Ok(self.sender.0.kv_op(self.name, op, key, value)?)
    }

    /// The bytes stored under `key`.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, crate::kv::KvError> {
        match self.op(bulkhead_native::hot::KvOp::Get, key, &[])? {
            p::KvAnswer::Value(v) => Ok(v.map(|b| b.0)),
            other => Err(unexpected(&other)),
        }
    }

    /// Store `value` under `key`.
    ///
    /// # Errors
    ///
    /// The host's error, including [`crate::kv::KvError::Other`] past
    /// the bucket's quota.
    pub fn set(&mut self, key: &str, value: &[u8]) -> Result<(), crate::kv::KvError> {
        self.op(bulkhead_native::hot::KvOp::Set, key, value)
            .map(|_| ())
    }

    /// Remove `key`; removing a missing key is not an error.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn delete(&mut self, key: &str) -> Result<(), crate::kv::KvError> {
        self.op(bulkhead_native::hot::KvOp::Delete, key, &[])
            .map(|_| ())
    }

    /// Whether `key` is set.
    ///
    /// # Errors
    ///
    /// The host's error.
    pub fn exists(&mut self, key: &str) -> Result<bool, crate::kv::KvError> {
        match self.op(bulkhead_native::hot::KvOp::Exists, key, &[])? {
            p::KvAnswer::Exists(e) => Ok(e),
            other => Err(unexpected(&other)),
        }
    }

    /// Add `delta` to the counter under `key` (a missing key counts from
    /// zero) and return the new count.
    ///
    /// # Errors
    ///
    /// The host's error, [`crate::kv::KvError::Other`] if the value is
    /// not a count.
    pub fn increment(&mut self, key: &str, delta: u64) -> Result<u64, crate::kv::KvError> {
        match self.sender.0.kv(p::KvRequest::Increment {
            bucket: self.name.to_string(),
            key: key.to_string(),
            delta,
        })? {
            p::KvAnswer::Count(n) => Ok(n),
            other => Err(unexpected(&other)),
        }
    }
}

#[cfg(feature = "keyvalue")]
fn unexpected(answer: &p::KvAnswer) -> crate::kv::KvError {
    crate::kv::KvError::Other(format!("unexpected keyvalue answer {answer:?}"))
}
