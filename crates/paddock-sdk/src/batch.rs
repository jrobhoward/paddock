//! Many small updates in one message.
//!
//! A cast costs about the same whatever it carries (about 1.3 µs handled,
//! BASELINE.md), so an app that sends many small updates (points, events,
//! rows) to the same instance sends far more of them per second in
//! batches. A [`Batcher`] collects items for each destination and hands
//! back one envelope when a batch is full ([`Limits::max_items`],
//! [`Limits::max_bytes`]) or, with [`Limits::max_delay`], when its oldest
//! item has waited that long. The receiver reads the items back with
//! [`items`].
//!
//! ```ignore
//! use paddock_sdk::batch::{self, Batcher, Limits};
//!
//! // In the sender's state:
//! let mut out: Batcher<Pid, Point> = Batcher::new(Limits {
//!     max_items: 500,
//!     max_bytes: 64 * 1024,
//!     max_delay: Some(Duration::from_millis(20)),
//! });
//!
//! // For each update:
//! for (to, msg) in out.push(ctx, collector, &point)? {
//!     ctx.cast(to, msg);
//! }
//!
//! // In handle_info, for every timer:
//! if let Info::Timer { timer, .. } = info {
//!     if let Some((to, msg)) = out.on_timer(timer) {
//!         ctx.cast(to, msg);
//!     }
//! }
//!
//! // In the receiver's handle_cast:
//! if let Some(points) = batch::items::<Point>(&msg) {
//!     for point in points? { /* ... */ }
//! }
//! ```
//!
//! A batch is one message: it is delivered or dropped whole (a full
//! mailbox drops it), counts once against the receiver's mailbox, and must
//! fit `limits.message_max_bytes`. Items still buffered when the sender
//! exits are lost, as unsent messages are; flush them in `terminate` with
//! [`Batcher::flush_all`] where that matters.
//!
//! The wire format is a CBOR array of the items, content type
//! `application/cbor`, schema [`SCHEMA`]; a receiver in another language
//! reads it as a CBOR array.

use std::collections::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::envelope::{CBOR, Envelope, MessageError};
use crate::{Ctx, TimerRef};

/// The schema id of a batch.
pub const SCHEMA: &str = "paddock/batch";

/// When a batch goes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Items in a full batch (at least 1).
    pub max_items: usize,
    /// Encoded bytes in a full batch. An item larger than this goes out in
    /// a batch of its own.
    pub max_bytes: usize,
    /// Most time an item waits before its batch goes out anyway; `None`
    /// for no timer (flush with [`Batcher::flush`] or
    /// [`Batcher::flush_all`]).
    pub max_delay: Option<Duration>,
}

impl Default for Limits {
    /// 1,000 items, 64 KiB, 10 ms.
    fn default() -> Self {
        Self {
            max_items: 1_000,
            max_bytes: 64 * 1024,
            max_delay: Some(Duration::from_millis(10)),
        }
    }
}

/// One destination's items, encoded as they arrive.
#[derive(Debug, Default)]
struct Pending {
    count: usize,
    body: Vec<u8>,
    timer: Option<TimerRef>,
}

/// Arms and cancels the timers that flush partial batches: [`Ctx`] in an
/// app, a fake in tests.
trait Timers {
    fn arm(&mut self, delay: Duration) -> TimerRef;
    fn cancel(&mut self, timer: TimerRef);
}

impl Timers for Ctx {
    fn arm(&mut self, delay: Duration) -> TimerRef {
        // The timer's own message is not read: `on_timer` goes by the
        // timer reference.
        self.send_after(delay, Envelope::new(CBOR, Vec::new()).with_schema(SCHEMA))
    }

    fn cancel(&mut self, timer: TimerRef) {
        self.cancel_timer(timer);
    }
}

/// Items of type `T` collected into batches, one batch per destination `K`
/// (usually a [`Pid`](crate::Pid)).
#[derive(Debug)]
pub struct Batcher<K, T> {
    limits: Limits,
    pending: HashMap<K, Pending>,
    timers: HashMap<TimerRef, K>,
    _items: PhantomData<fn(&T)>,
}

impl<K: Eq + Hash + Clone, T: Serialize> Batcher<K, T> {
    /// An empty batcher.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits: Limits {
                max_items: limits.max_items.max(1),
                ..limits
            },
            pending: HashMap::new(),
            timers: HashMap::new(),
            _items: PhantomData,
        }
    }

    /// Add `item` to `to`'s batch. Returns the batches to send now: none
    /// while the batch fills, one when it is full, and two when `item`
    /// would take it past `max_bytes` and also fills the next on its own.
    /// With [`Limits::max_delay`], the first item of a batch arms a timer
    /// for [`Batcher::on_timer`]. The returned `Vec` does not allocate when
    /// empty.
    ///
    /// # Errors
    ///
    /// [`MessageError`] if `item` cannot be encoded; the batch is unchanged.
    pub fn push(
        &mut self,
        ctx: &mut Ctx,
        to: K,
        item: &T,
    ) -> Result<Vec<(K, Envelope)>, MessageError> {
        self.push_with(ctx, to, item)
    }

    fn push_with(
        &mut self,
        timers: &mut impl Timers,
        to: K,
        item: &T,
    ) -> Result<Vec<(K, Envelope)>, MessageError> {
        let mut encoded = Vec::new();
        ciborium::into_writer(item, &mut encoded)
            .map_err(|e| MessageError(format!("CBOR encode: {e}")))?;
        // An item that would take the batch past `max_bytes` sends what is
        // there first.
        let mut out = Vec::new();
        if self
            .pending
            .get(&to)
            .is_some_and(|p| p.count > 0 && p.body.len() + encoded.len() > self.limits.max_bytes)
        {
            out.extend(self.take(timers, &to));
        }
        let pending = self.pending.entry(to.clone()).or_default();
        pending.body.extend_from_slice(&encoded);
        pending.count += 1;
        let full =
            pending.count >= self.limits.max_items || pending.body.len() >= self.limits.max_bytes;
        if !full
            && pending.timer.is_none()
            && let Some(delay) = self.limits.max_delay
        {
            let timer = timers.arm(delay);
            pending.timer = Some(timer);
            self.timers.insert(timer, to.clone());
        }
        if full {
            out.extend(self.take(timers, &to));
        }
        Ok(out)
    }

    /// A timer fired: the batch it was armed for, if it is one of this
    /// batcher's and that batch still has items. Call it for every
    /// [`Info::Timer`](crate::Info::Timer); other timers give `None`.
    pub fn on_timer(&mut self, timer: TimerRef) -> Option<(K, Envelope)> {
        let to = self.timers.remove(&timer)?;
        let pending = self.pending.get_mut(&to)?;
        if pending.timer != Some(timer) {
            return None;
        }
        pending.timer = None;
        let msg = seal(pending)?;
        Some((to, msg))
    }

    /// `to`'s batch now, if it has items.
    pub fn flush(&mut self, ctx: &mut Ctx, to: &K) -> Option<Envelope> {
        self.take(ctx, to).map(|(_, msg)| msg)
    }

    /// Every batch with items, now.
    pub fn flush_all(&mut self, ctx: &mut Ctx) -> Vec<(K, Envelope)> {
        let keys: Vec<K> = self.pending.keys().cloned().collect();
        keys.iter().filter_map(|k| self.take(ctx, k)).collect()
    }

    /// Items waiting for `to`.
    #[must_use]
    pub fn waiting(&self, to: &K) -> usize {
        self.pending.get(to).map_or(0, |p| p.count)
    }

    /// Take `to`'s batch and cancel its timer.
    fn take(&mut self, timers: &mut impl Timers, to: &K) -> Option<(K, Envelope)> {
        let pending = self.pending.get_mut(to)?;
        if let Some(timer) = pending.timer.take() {
            timers.cancel(timer);
            self.timers.remove(&timer);
        }
        let msg = seal(pending)?;
        Some((to.clone(), msg))
    }
}

/// The batch's envelope, leaving `pending` empty; `None` without items.
fn seal(pending: &mut Pending) -> Option<Envelope> {
    if pending.count == 0 {
        return None;
    }
    let mut body = array_header(pending.count);
    body.append(&mut pending.body);
    pending.count = 0;
    Some(Envelope::new(CBOR, body).with_schema(SCHEMA))
}

/// The CBOR head of a definite-length array of `n` items (major type 4).
fn array_header(n: usize) -> Vec<u8> {
    let n = n as u64;
    match n {
        0..=23 => vec![0x80 | n as u8],
        24..=0xff => vec![0x98, n as u8],
        0x100..=0xffff => {
            let mut h = vec![0x99];
            h.extend_from_slice(&(n as u16).to_be_bytes());
            h
        }
        0x1_0000..=0xffff_ffff => {
            let mut h = vec![0x9a];
            h.extend_from_slice(&(n as u32).to_be_bytes());
            h
        }
        _ => {
            let mut h = vec![0x9b];
            h.extend_from_slice(&n.to_be_bytes());
            h
        }
    }
}

/// Encode `items` as one batch, for a sender that collects them itself.
///
/// # Errors
///
/// [`MessageError`] if an item cannot be encoded.
pub fn envelope<T: Serialize>(items: &[T]) -> Result<Envelope, MessageError> {
    Envelope::cbor(items).map(|e| e.with_schema(SCHEMA))
}

/// The items of `msg`, if it is a batch ([`SCHEMA`]); `None` for any other
/// message.
#[must_use]
pub fn items<T: DeserializeOwned>(msg: &Envelope) -> Option<Result<Vec<T>, MessageError>> {
    (msg.schema.as_deref() == Some(SCHEMA)).then(|| msg.decode())
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod batch_tests;
