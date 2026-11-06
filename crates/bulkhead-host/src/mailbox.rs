//! An instance's mailbox: a bounded channel of messages, limited both in
//! count (`mailbox_max`) and in the bytes of the bodies it holds
//! (`mailbox_max_bytes`, ROADMAP I6).
//!
//! The byte limit is a semaphore with a permit per byte. A send takes its
//! body's bytes before it takes a slot, and the permit travels with the
//! message, so the bytes come back as soon as the instance takes the
//! message, or when it is dropped with the mailbox. A message that finds
//! either limit reached is refused as at a full mailbox
//! ([`TrySendError::Full`]), or, by [`Sender::send_timeout`] (a native
//! app's `cast_timeout`, ROADMAP I2), waits for room: first for its bytes,
//! then for a slot. The semaphore and the channel both serve waiters in
//! arrival order, and a send that does not wait never takes room a waiter
//! is owed.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::instance::Message;
use crate::message::Info;

/// A message in the channel, holding its body's bytes of the budget.
#[derive(Debug)]
struct Queued {
    message: Message,
    _bytes: OwnedSemaphorePermit,
}

/// The byte budget of one mailbox.
#[derive(Debug)]
struct Budget {
    permits: Arc<Semaphore>,
    /// The permits the semaphore was given in all: the limit, clamped to
    /// what a semaphore holds.
    max: AtomicU64,
}

impl Budget {
    fn new(max: u64) -> Self {
        let max = clamp(max);
        Self {
            permits: Arc::new(Semaphore::new(usize::try_from(max).unwrap_or(0))),
            max: AtomicU64::new(max),
        }
    }

    /// Take `bytes` of the budget, if that many are free.
    fn take(&self, bytes: usize) -> Option<OwnedSemaphorePermit> {
        let n = u32::try_from(bytes).ok()?;
        self.permits.clone().try_acquire_many_owned(n).ok()
    }
}

/// A limit as a number of permits a semaphore can hold.
fn clamp(max: u64) -> u64 {
    max.min(u64::try_from(Semaphore::MAX_PERMITS).unwrap_or(u64::MAX))
}

/// The bytes a message holds of its mailbox's budget: its body's. Only
/// timer messages arrive as info through the mailbox; the rest hold none.
fn size(message: &Message) -> usize {
    match message {
        Message::Call { msg, .. } | Message::Cast(msg) => msg.body.len(),
        Message::Info(Info::Timer { msg, .. }) => msg.body.len(),
        Message::Info(_) => 0,
    }
}

/// A mailbox of at most `max` messages and `max_bytes` bytes of bodies.
pub(crate) fn channel(max: usize, max_bytes: u64) -> (Sender, Receiver) {
    let (tx, rx) = mpsc::channel(max.max(1));
    let budget = Arc::new(Budget::new(max_bytes));
    (Sender { tx, budget }, Receiver { rx })
}

/// Why [`Sender::send_timeout`] did not queue its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitError {
    /// The body is larger than the mailbox's byte limit: it never fits.
    Full,
    /// The mailbox closed: the instance exited.
    Closed,
    /// No room came within the timeout. The message was not queued.
    Timeout,
}

/// The sending side of a mailbox.
#[derive(Debug, Clone)]
pub(crate) struct Sender {
    tx: mpsc::Sender<Queued>,
    budget: Arc<Budget>,
}

impl Sender {
    /// Queue `message` if the mailbox has a slot and room for its body.
    pub(crate) fn try_send(&self, message: Message) -> Result<(), TrySendError<Message>> {
        let Some(bytes) = self.budget.take(size(&message)) else {
            return Err(if self.tx.is_closed() {
                TrySendError::Closed(message)
            } else {
                TrySendError::Full(message)
            });
        };
        self.tx
            .try_send(Queued {
                message,
                _bytes: bytes,
            })
            .map_err(|e| match e {
                TrySendError::Full(q) => TrySendError::Full(q.message),
                TrySendError::Closed(q) => TrySendError::Closed(q.message),
            })
    }

    /// Queue `message`, waiting up to `timeout` for a slot and room for its
    /// body if the mailbox has neither now.
    pub(crate) async fn send_timeout(
        &self,
        message: Message,
        timeout: Duration,
    ) -> Result<(), WaitError> {
        let message = match self.try_send(message) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Closed(_)) => return Err(WaitError::Closed),
            Err(TrySendError::Full(message)) => message,
        };
        let n = u32::try_from(size(&message))
            .ok()
            .filter(|n| u64::from(*n) <= self.budget.max.load(Ordering::Relaxed))
            .ok_or(WaitError::Full)?;
        let wait = async {
            let bytes = self
                .budget
                .permits
                .clone()
                .acquire_many_owned(n)
                .await
                .map_err(|_| WaitError::Closed)?;
            let slot = self.tx.reserve().await.map_err(|_| WaitError::Closed)?;
            slot.send(Queued {
                message,
                _bytes: bytes,
            });
            Ok(())
        };
        tokio::time::timeout(timeout, wait)
            .await
            .unwrap_or(Err(WaitError::Timeout))
    }

    /// A sender that does not keep the mailbox open, for timers.
    pub(crate) fn downgrade(&self) -> WeakSender {
        WeakSender {
            tx: self.tx.downgrade(),
            budget: self.budget.clone(),
        }
    }

    /// Messages waiting.
    pub(crate) fn queued(&self) -> usize {
        self.tx.max_capacity().saturating_sub(self.tx.capacity())
    }

    /// Bytes of bodies waiting.
    pub(crate) fn queued_bytes(&self) -> u64 {
        let free = u64::try_from(self.budget.permits.available_permits()).unwrap_or(u64::MAX);
        self.budget.max.load(Ordering::Relaxed).saturating_sub(free)
    }

    /// Raise the byte limit to `max_bytes` (an upgrade to a version that
    /// asks for more). A lower limit takes effect at the next start.
    pub(crate) fn raise_max_bytes(&self, max_bytes: u64) {
        let max = clamp(max_bytes);
        let before = self.budget.max.fetch_max(max, Ordering::Relaxed);
        if max > before {
            self.budget
                .permits
                .add_permits(usize::try_from(max - before).unwrap_or(0));
        }
    }
}

/// A [`Sender`] that does not keep the mailbox open.
#[derive(Debug, Clone)]
pub(crate) struct WeakSender {
    tx: mpsc::WeakSender<Queued>,
    budget: Arc<Budget>,
}

impl WeakSender {
    /// The sender, while the mailbox is open.
    pub(crate) fn upgrade(&self) -> Option<Sender> {
        Some(Sender {
            tx: self.tx.upgrade()?,
            budget: self.budget.clone(),
        })
    }
}

/// The receiving side of a mailbox, held by the instance's task.
#[derive(Debug)]
pub(crate) struct Receiver {
    rx: mpsc::Receiver<Queued>,
}

impl Receiver {
    /// The next message, giving its bytes back; `None` once the mailbox is
    /// closed and empty.
    pub(crate) async fn recv(&mut self) -> Option<Message> {
        self.rx.recv().await.map(|q| q.message)
    }

    /// The next message, if one is waiting.
    pub(crate) fn try_recv(&mut self) -> Result<Message, TryRecvError> {
        self.rx.try_recv().map(|q| q.message)
    }

    /// Refuse further messages; those waiting can still be taken.
    pub(crate) fn close(&mut self) {
        self.rx.close();
    }

    /// Whether no message is waiting.
    pub(crate) fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }
}

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod mailbox_tests;
