//! An instance's mailbox: a bounded channel of messages, limited both in
//! count (`mailbox_max`) and in the bytes of the bodies it holds
//! (`mailbox_max_bytes`, ROADMAP I6).
//!
//! The byte limit is a counter of the bytes queued. A send adds its body's
//! bytes before it takes a slot, and the receiver takes them off as it
//! takes the message, or as it drops the messages left in a closed mailbox.
//! A message that finds either limit reached is refused as at a full
//! mailbox ([`TrySendError::Full`]), or, by [`Sender::send_timeout`] (a
//! native app's `cast_timeout`, ROADMAP I2), waits for room: first for its
//! bytes, then for a slot.
//!
//! Waiters take turns in arrival order (a fair lock), the first waiting on
//! the receiver's notice that bytes came back; the channel serves those
//! waiting for a slot in arrival order too. While anyone waits for bytes,
//! a send that does not wait is refused even if its body would fit, so it
//! never takes room a waiter is owed. A send that does not wait costs one
//! compare-and-swap on the counter and the receiver one subtraction (ROADMAP
//! I9; I6's semaphore cost unbatched casts 3 to 6% of their rate).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{Mutex, Notify, mpsc};

use crate::instance::Message;
use crate::message::Info;

/// A message in the channel, with the bytes it holds of the budget.
#[derive(Debug)]
struct Queued {
    message: Message,
    bytes: u64,
}

/// The byte budget of one mailbox.
#[derive(Debug)]
struct Budget {
    /// Bytes of bodies queued, or taken by a waiter about to queue.
    used: AtomicU64,
    /// The limit.
    max: AtomicU64,
    /// Senders waiting for bytes: while not zero, a send that does not wait
    /// is refused.
    waiters: AtomicUsize,
    /// The waiters' turns, in arrival order.
    turn: Mutex<()>,
    /// Bytes came back, the limit rose, or the mailbox closed: wakes the
    /// waiter whose turn it is.
    freed: Notify,
}

impl Budget {
    fn new(max: u64) -> Self {
        Self {
            used: AtomicU64::new(0),
            max: AtomicU64::new(max),
            waiters: AtomicUsize::new(0),
            turn: Mutex::new(()),
            freed: Notify::new(),
        }
    }

    /// Take `bytes` of the budget for a send that does not wait: only if
    /// they are free and no one is waiting for bytes. An empty body takes
    /// none, and is limited by count only.
    fn try_take(&self, bytes: u64) -> bool {
        bytes == 0 || (self.waiters.load(Ordering::SeqCst) == 0 && self.take(bytes))
    }

    /// Take `bytes` of the budget, if that many are free.
    fn take(&self, bytes: u64) -> bool {
        let max = self.max.load(Ordering::Relaxed);
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let Some(after) = used.checked_add(bytes).filter(|after| *after <= max) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(used, after, Ordering::SeqCst, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(now) => used = now,
            }
        }
    }

    /// Give `bytes` back, waking a waiter if there is one.
    fn give(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.used.fetch_sub(bytes, Ordering::SeqCst);
        // Paired with the waiter's increment of `waiters` before it tries to
        // take: either it sees these bytes free, or this sees it waiting.
        if self.waiters.load(Ordering::SeqCst) != 0 {
            self.freed.notify_one();
        }
    }

    /// Take `bytes` of the budget, waiting in turn until they are free or
    /// `closed` says the mailbox closed (`false`).
    async fn wait_take(&self, bytes: u64, closed: impl Fn() -> bool) -> bool {
        struct Waiting<'a>(&'a AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.waiters.fetch_add(1, Ordering::SeqCst);
        let _waiting = Waiting(&self.waiters);
        let _turn = self.turn.lock().await;
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if closed() {
                return false;
            }
            if self.take(bytes) {
                return true;
            }
            freed.await;
        }
    }
}

/// The bytes a message holds of its mailbox's budget: its body's. Only
/// timer messages arrive as info through the mailbox; the rest hold none.
fn size(message: &Message) -> u64 {
    let len = match message {
        Message::Call { msg, .. } | Message::Cast(msg) => msg.body.len(),
        Message::Info(Info::Timer { msg, .. }) => msg.body.len(),
        Message::Info(_) => 0,
    };
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// A mailbox of at most `max` messages and `max_bytes` bytes of bodies.
pub(crate) fn channel(max: usize, max_bytes: u64) -> (Sender, Receiver) {
    let (tx, rx) = mpsc::channel(max.max(1));
    let budget = Arc::new(Budget::new(max_bytes));
    (
        Sender {
            tx,
            budget: budget.clone(),
        },
        Receiver { rx, budget },
    )
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
        let bytes = size(&message);
        if !self.budget.try_take(bytes) {
            return Err(if self.tx.is_closed() {
                TrySendError::Closed(message)
            } else {
                TrySendError::Full(message)
            });
        }
        self.tx.try_send(Queued { message, bytes }).map_err(|e| {
            self.budget.give(bytes);
            match e {
                TrySendError::Full(q) => TrySendError::Full(q.message),
                TrySendError::Closed(q) => TrySendError::Closed(q.message),
            }
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
        let bytes = size(&message);
        if bytes > self.budget.max.load(Ordering::Relaxed) {
            return Err(WaitError::Full);
        }
        let wait = async {
            if bytes != 0 && !self.budget.wait_take(bytes, || self.tx.is_closed()).await {
                return Err(WaitError::Closed);
            }
            // The bytes are held from here: give them back unless queued.
            let held = Held {
                budget: &self.budget,
                bytes,
            };
            let slot = self.tx.reserve().await.map_err(|_| WaitError::Closed)?;
            slot.send(Queued {
                message,
                bytes: held.queued(),
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
        self.budget.used.load(Ordering::Relaxed)
    }

    /// Raise the byte limit to `max_bytes` (an upgrade to a version that
    /// asks for more). A lower limit takes effect at the next start.
    pub(crate) fn raise_max_bytes(&self, max_bytes: u64) {
        let before = self.budget.max.fetch_max(max_bytes, Ordering::SeqCst);
        if max_bytes > before {
            self.budget.freed.notify_one();
        }
    }
}

/// Bytes a waiter took for a message not yet queued, given back if the
/// wait for a slot ends without one.
struct Held<'a> {
    budget: &'a Budget,
    bytes: u64,
}

impl Held<'_> {
    /// The bytes, now the queued message's to give back.
    fn queued(mut self) -> u64 {
        std::mem::take(&mut self.bytes)
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.budget.give(self.bytes);
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
    budget: Arc<Budget>,
}

impl Receiver {
    /// The message, its bytes given back.
    fn taken(&self, q: Queued) -> Message {
        self.budget.give(q.bytes);
        q.message
    }

    /// The next message, giving its bytes back; `None` once the mailbox is
    /// closed and empty.
    pub(crate) async fn recv(&mut self) -> Option<Message> {
        let q = self.rx.recv().await?;
        Some(self.taken(q))
    }

    /// The next message, if one is waiting.
    pub(crate) fn try_recv(&mut self) -> Result<Message, TryRecvError> {
        let q = self.rx.try_recv()?;
        Ok(self.taken(q))
    }

    /// Refuse further messages; those waiting can still be taken. A sender
    /// waiting for bytes gives up.
    pub(crate) fn close(&mut self) {
        self.rx.close();
        self.budget.freed.notify_one();
    }

    /// Whether no message is waiting.
    pub(crate) fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }
}

impl Drop for Receiver {
    /// The messages left are dropped with their bytes given back, and a
    /// sender waiting for bytes gives up.
    fn drop(&mut self) {
        self.rx.close();
        while let Ok(q) = self.rx.try_recv() {
            self.budget.give(q.bytes);
        }
        self.budget.freed.notify_one();
    }
}

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod mailbox_tests;
