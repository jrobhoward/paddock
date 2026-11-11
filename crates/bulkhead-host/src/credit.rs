//! Credit for waited casts between nodes (ROADMAP I11 to I14).
//!
//! Between nodes a waited cast (`cast_timeout`) costs its sender a round
//! trip: the target's node answers once the target's mailbox took it. With
//! credit, the target's node reserves room in the target's mailbox for one
//! sender ahead of time ([`crate::mailbox::Reservation`]), and while the
//! sender holds credit its own node answers `Ok` at once and sends the cast
//! on as [`Frame::CastCredited`], which the reservation always takes.
//!
//! - **Granted** by the target's node after a [`Frame::CastWait`] from that
//!   sender to that target was queued: as much as the mailbox gives without
//!   waiting, up to the node's `cast_credit` messages and
//!   `cast_credit_bytes` ([`Frame::Credit`]). Topped up as credited casts
//!   arrive, once half of the window is used.
//! - **Held** per sending instance and target, so the order between a pair
//!   and the messaging ACL's verdict hold per pair. A cast whose body does
//!   not fit what is left goes as a [`Frame::CastWait`], which renews the
//!   grant.
//! - **Ended** when the target exits or refuses the sender
//!   ([`Frame::CreditVoid`]), when the sender exits or leaves its credit
//!   unused for [`IDLE`] ([`Frame::CreditReturn`], sent behind the casts
//!   that used the rest, since a link keeps its order), and when the session
//!   between the two nodes ends (both sides drop it).
//!
//! `Ok` for a credited cast means room was reserved for it and it is on
//! the link: it is lost only if the session ends before it arrives, the
//! target exits first, or an ACL changed since the grant refuses it. A
//! credited cast that finds its credit voided or returned meanwhile is
//! queued as a plain cast, if there is room.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::time::Instant;

use crate::delivery::{self, Route};
use crate::instance::InstanceHandle;
use crate::mailbox::Reservation;
use crate::message::Envelope;
use crate::pid::Pid;
use crate::runtime::RuntimeInner;
use crate::transport::{self, Frame};

/// Credit unused for this long goes back to the target's node.
pub(crate) const IDLE: Duration = Duration::from_secs(1);

/// A sender and a target.
type Key = (Pid, Pid);

const SHARDS: usize = 16;

/// Maps behind a lock each, picked by key, so senders seldom share one.
#[derive(Debug)]
struct Shards<T> {
    hasher: RandomState,
    shards: [Mutex<HashMap<Key, T>>; SHARDS],
}

impl<T> Default for Shards<T> {
    fn default() -> Self {
        Self {
            hasher: RandomState::new(),
            shards: std::array::from_fn(|_| Mutex::new(HashMap::new())),
        }
    }
}

impl<T> Shards<T> {
    fn lock(&self, key: &Key) -> MutexGuard<'_, HashMap<Key, T>> {
        let i = usize::try_from(self.hasher.hash_one(key) % SHARDS as u64).unwrap_or(0);
        self.shards[i]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Remove and return every entry `pick` chooses.
    fn drain(&self, mut pick: impl FnMut(&Key, &T) -> bool) -> Vec<(Key, T)> {
        let mut out = Vec::new();
        for shard in &self.shards {
            let mut map = shard.lock().unwrap_or_else(PoisonError::into_inner);
            let keys: Vec<Key> = map
                .iter()
                .filter(|(k, v)| pick(k, v))
                .map(|(k, _)| *k)
                .collect();
            for key in keys {
                if let Some(v) = map.remove(&key) {
                    out.push((key, v));
                }
            }
        }
        out
    }
}

/// Credit a local sender holds for a target on another node.
#[derive(Debug)]
struct Held {
    messages: u32,
    bytes: u64,
    message_max: u64,
    last_used: Instant,
}

/// A node's credit, both ways.
#[derive(Debug)]
pub(crate) struct Credits {
    /// Casts per grant; 0 grants none.
    window: u32,
    /// Bytes of bodies per grant.
    window_bytes: u64,
    /// Room reserved in local targets' mailboxes for senders elsewhere.
    granted: Shards<Reservation>,
    /// Credit local senders hold for targets elsewhere.
    held: Shards<Held>,
    sweeping: AtomicBool,
}

impl Credits {
    pub(crate) fn new(window: u32, window_bytes: u64) -> Self {
        Self {
            window,
            window_bytes,
            granted: Shards::default(),
            held: Shards::default(),
            sweeping: AtomicBool::new(false),
        }
    }

    /// Take one cast of `bytes` from `from`'s credit for `to`, if it holds
    /// enough.
    fn take(&self, from: Pid, to: Pid, bytes: u64) -> bool {
        let key = (from, to);
        let mut map = self.held.lock(&key);
        let Some(held) = map.get_mut(&key) else {
            return false;
        };
        if held.messages == 0 || bytes > held.bytes || bytes > held.message_max {
            return false;
        }
        held.messages -= 1;
        held.bytes -= bytes;
        held.last_used = Instant::now();
        true
    }

    /// Give back what [`Credits::take`] took, for a cast the link refused.
    fn untake(&self, from: Pid, to: Pid, bytes: u64) {
        let key = (from, to);
        if let Some(held) = self.held.lock(&key).get_mut(&key) {
            held.messages += 1;
            held.bytes += bytes;
        }
    }

    fn add(&self, from: Pid, to: Pid, messages: u32, bytes: u64, message_max: u64) {
        let key = (from, to);
        let mut map = self.held.lock(&key);
        let held = map.entry(key).or_insert(Held {
            messages: 0,
            bytes: 0,
            message_max,
            last_used: Instant::now(),
        });
        held.messages = held.messages.saturating_add(messages);
        held.bytes = held.bytes.saturating_add(bytes);
        held.message_max = message_max;
        held.last_used = Instant::now();
    }

    /// The casts and bytes `from` holds for `to`, for tests.
    #[cfg(test)]
    pub(crate) fn held(&self, from: Pid, to: Pid) -> Option<(u32, u64)> {
        let key = (from, to);
        self.held
            .lock(&key)
            .get(&key)
            .map(|h| (h.messages, h.bytes))
    }
}

fn as_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn as_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Send a waited cast from local `from` to `to` on `node` on credit, if
/// `from` holds enough: `None` if not, and the message comes back for a
/// [`Frame::CastWait`]. `Ok` once the link took it.
pub(crate) fn send(
    rt: &RuntimeInner,
    node: u32,
    from: Pid,
    sender: Option<crate::acl::Stamp>,
    to: Pid,
    msg: Envelope,
) -> Result<Result<(), crate::CallError>, Envelope> {
    let bytes = u64::try_from(msg.body.len()).unwrap_or(u64::MAX);
    if !rt.credits.take(from, to, bytes) {
        return Err(msg);
    }
    let frame = Frame::CastCredited {
        from,
        sender,
        to,
        msg,
    };
    Ok(transport::try_send(rt, node, frame).map_err(|why| {
        rt.credits.untake(from, to, bytes);
        transport::refused(why)
    }))
}

/// A waited cast from `from` on `node` was queued in local `to`'s mailbox:
/// grant `from` credit for `to`, or top it up.
pub(crate) fn grant(rt: &RuntimeInner, node: u32, from: Pid, to: Pid) {
    let credits = &rt.credits;
    if credits.window == 0 {
        return;
    }
    let Route::Local(target) = delivery::route(rt, to) else {
        return;
    };
    let key = (from, to);
    let mut map = credits.granted.lock(&key);
    let added = match map.get_mut(&key) {
        Some(reservation) => top_up(credits, &target, reservation),
        None => match target.reserve(as_usize(credits.window), credits.window_bytes) {
            Some(reservation) => {
                let added = (reservation.slots(), reservation.bytes());
                map.insert(key, reservation);
                added
            }
            None => (0, 0),
        },
    };
    offer(rt, node, &target, &mut map, key, added);
}

/// Top `reservation` up to the window.
fn top_up(
    credits: &Credits,
    target: &InstanceHandle,
    reservation: &mut Reservation,
) -> (usize, u64) {
    let slots = as_usize(credits.window).saturating_sub(reservation.slots());
    let bytes = credits.window_bytes.saturating_sub(reservation.bytes());
    if slots == 0 {
        return (0, 0);
    }
    target.top_up(reservation, slots, bytes)
}

/// Tell `node` of credit just added; if it cannot be told (a peer at an
/// older version, or unreachable), give the room back.
fn offer(
    rt: &RuntimeInner,
    node: u32,
    target: &InstanceHandle,
    map: &mut HashMap<Key, Reservation>,
    key: Key,
    (slots, bytes): (usize, u64),
) {
    if slots == 0 && bytes == 0 {
        return;
    }
    let frame = Frame::Credit {
        from: key.0,
        to: key.1,
        messages: as_u32(slots),
        bytes,
        message_max: target.message_max_bytes(),
    };
    if transport::try_send(rt, node, frame).is_err()
        && let Some(reservation) = map.get_mut(&key)
    {
        reservation.release(slots, bytes);
        if reservation.slots() == 0 {
            map.remove(&key);
        }
    }
}

/// A cast `from` on `node` sent on credit arrived for local `to`: queue it
/// into the room reserved for it, and top the credit up once half is used.
/// If `to` is gone or no room is reserved, the credit is void.
pub(crate) fn arrived(rt: &RuntimeInner, node: u32, from: Pid, to: Pid, msg: Envelope) {
    let credits = &rt.credits;
    let key = (from, to);
    let Route::Local(target) = delivery::route(rt, to) else {
        credits.granted.lock(&key).remove(&key);
        void(rt, node, from, to);
        return;
    };
    let mut map = credits.granted.lock(&key);
    let Some(reservation) = map.get_mut(&key) else {
        // Voided or returned meanwhile: queued if there is room, as a cast.
        drop(map);
        let _ = target.cast_from(Some(from), msg);
        void(rt, node, from, to);
        return;
    };
    let bytes = u64::try_from(msg.body.len()).unwrap_or(u64::MAX);
    if target.cast_credited(from, msg, reservation).is_err() {
        // The sender counted it as used: so does the reservation.
        reservation.release(1, bytes);
    }
    let low = reservation.slots() <= as_usize(credits.window / 2)
        || reservation.bytes() <= credits.window_bytes / 2;
    if low {
        let added = top_up(credits, &target, reservation);
        offer(rt, node, &target, &mut map, key, added);
    }
}

/// `from`'s credit for `to` is void: tell `from`'s node.
fn void(rt: &RuntimeInner, node: u32, from: Pid, to: Pid) {
    let _ = transport::try_send(rt, node, Frame::CreditVoid { from, to });
}

/// A cast on credit that the messaging ACL refused: void the credit.
pub(crate) fn refused(rt: &RuntimeInner, node: u32, from: Pid, to: Pid) {
    let key = (from, to);
    rt.credits.granted.lock(&key).remove(&key);
    void(rt, node, from, to);
}

/// `node` granted local `from` credit for its `to`.
pub(crate) fn credited(
    rt: &Arc<RuntimeInner>,
    node: u32,
    from: Pid,
    to: Pid,
    messages: u32,
    bytes: u64,
    message_max: u64,
) {
    if !matches!(delivery::route(rt, from), Route::Local(_)) {
        let _ = transport::try_send(
            rt,
            node,
            Frame::CreditReturn {
                from,
                to,
                messages,
                bytes,
            },
        );
        return;
    }
    rt.credits.add(from, to, messages, bytes, message_max);
    if !rt.credits.sweeping.swap(true, Ordering::Relaxed) {
        tokio::spawn(sweep(Arc::downgrade(rt)));
    }
}

/// `node` voided local `from`'s credit for its `to`.
pub(crate) fn voided(rt: &RuntimeInner, from: Pid, to: Pid) {
    let key = (from, to);
    rt.credits.held.lock(&key).remove(&key);
}

/// `from` on `node` gave back credit for local `to`.
pub(crate) fn returned(rt: &RuntimeInner, from: Pid, to: Pid, messages: u32, bytes: u64) {
    let key = (from, to);
    let mut map = rt.credits.granted.lock(&key);
    if let Some(reservation) = map.get_mut(&key) {
        reservation.release(as_usize(messages), bytes);
        if reservation.slots() == 0 {
            map.remove(&key);
        }
    }
}

/// Give back every credit `pick` chooses to its target's node.
fn give_back(rt: &RuntimeInner, pick: impl FnMut(&Key, &Held) -> bool) {
    for ((from, to), held) in rt.credits.held.drain(pick) {
        let _ = transport::try_send(
            rt,
            to.node,
            Frame::CreditReturn {
                from,
                to,
                messages: held.messages,
                bytes: held.bytes,
            },
        );
    }
}

/// Local `pid` exited: give back the credit it held, and void the credit
/// granted for its mailbox.
pub(crate) fn exited(rt: &RuntimeInner, pid: Pid) {
    give_back(rt, |(from, _), _| *from == pid);
    for ((from, to), _) in rt.credits.granted.drain(|(_, to), _| *to == pid) {
        void(rt, from.node, from, to);
    }
}

/// The session with `node` ended: drop every credit with it, both ways.
pub(crate) fn forget_node(rt: &RuntimeInner, node: u32) {
    rt.credits.held.drain(|(_, to), _| to.node == node);
    rt.credits.granted.drain(|(from, _), _| from.node == node);
}

/// Give back credit left unused for [`IDLE`], every half of it, while the
/// runtime lives.
async fn sweep(rt: Weak<RuntimeInner>) {
    let mut ticker = tokio::time::interval(IDLE / 2);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(rt) = rt.upgrade() else {
            return;
        };
        let now = Instant::now();
        give_back(&rt, |_, held| now.duration_since(held.last_used) >= IDLE);
    }
}

#[cfg(test)]
#[path = "credit_tests.rs"]
mod credit_tests;
