//! Node-to-node delivery: frames, the transport that carries them, and what
//! a runtime does with frames from other nodes.
//!
//! A [`Transport`] sends [`Packet`]s to other nodes, at most once and in
//! order per pair of nodes, and may report a node it can no longer reach
//! with [`Runtime::node_down`](crate::Runtime::node_down). Packets from
//! other nodes arrive through [`Runtime::deliver`](crate::Runtime::deliver).
//! [`memory`] is an in-process transport for tests, with faults to inject;
//! QUIC (ROADMAP D1) implements the same trait.
//!
//! Each node keeps its own side of a link or monitor between nodes. When the
//! session with a node ends, because the transport reported it lost or
//! because [`liveness`] stopped hearing it, its links and monitors end with
//! `noconnection` on this side and calls waiting on it fail with
//! `noconnection` (DESIGN.md, "Failure handling"). Every packet carries the
//! session it belongs to, so both sides end a session together and nothing
//! from an ended session is acted on.

pub mod liveness;
pub mod memory;

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::delivery::{self, Route};
use crate::message::{CallError, Envelope, ExitReason, Info};
use crate::pid::Pid;
use crate::probe;
use crate::runtime::RuntimeInner;
use liveness::{NodeEvent, Session};

/// A frame and the session it belongs to ([`liveness`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// The sender's session number with the receiving node.
    pub session: u64,
    /// The frame.
    pub frame: Frame,
}

/// One message between nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Frame {
    /// The sender is alive ([`liveness`]).
    Heartbeat {
        /// Whether the sender hears the receiving node.
        hears_you: bool,
    },
    /// A cast to `to`.
    Cast {
        /// The sender, if an instance.
        from: Option<Pid>,
        /// The target, on the receiving node.
        to: Pid,
        /// The message.
        msg: Envelope,
    },
    /// A call to `to`; the receiving node answers with [`Frame::Reply`].
    Call {
        /// The caller's node's id for this call.
        id: u64,
        /// The caller, if an instance.
        from: Option<Pid>,
        /// The target, on the receiving node.
        to: Pid,
        /// The message.
        msg: Envelope,
        /// How long the caller waits.
        timeout_ms: u64,
    },
    /// The answer to [`Frame::Call`] `id`.
    Reply {
        /// The call's id.
        id: u64,
        /// The reply, or why there is none.
        result: Result<Envelope, CallError>,
    },
    /// An explicit exit signal (`process.exit`).
    Exit {
        /// The sender.
        from: Pid,
        /// The target, on the receiving node.
        to: Pid,
        /// Why.
        reason: ExitReason,
        /// `killed`, which a trapping instance cannot catch.
        untrappable: bool,
    },
    /// `from` exited (or could not be linked) and was linked to `to`.
    LinkExit {
        /// The instance that exited.
        from: Pid,
        /// Its linked partner, on the receiving node.
        to: Pid,
        /// Why.
        reason: ExitReason,
    },
    /// Link `from` (on the sending node) with `to` (on the receiving node).
    Link {
        /// The instance that linked.
        from: Pid,
        /// The one it linked to.
        to: Pid,
    },
    /// Remove the link between `from` and `to`.
    Unlink {
        /// The instance that unlinked.
        from: Pid,
        /// Its partner.
        to: Pid,
    },
    /// `watcher` (on the sending node) monitors `target`.
    Monitor {
        /// The watcher.
        watcher: Pid,
        /// The target, on the receiving node.
        target: Pid,
        /// The reference the watcher's node made.
        reference: u64,
    },
    /// `watcher` stops monitoring with `reference`.
    Demonitor {
        /// The watcher.
        watcher: Pid,
        /// The reference.
        reference: u64,
    },
    /// `target` exited (or could not be monitored); tell `watcher`.
    Down {
        /// The monitor's reference.
        reference: u64,
        /// The instance that exited.
        target: Pid,
        /// The watcher, on the receiving node.
        watcher: Pid,
        /// Why.
        reason: ExitReason,
    },
}

/// Carries packets to other nodes.
pub trait Transport: Send + Sync + 'static {
    /// Send `packet` to `node`. Returns `false` if `node` cannot be reached
    /// now; the packet is then dropped.
    fn send(&self, node: u32, packet: Packet) -> bool;
}

/// Where a call's reply goes.
type ReplySender = oneshot::Sender<Result<Envelope, CallError>>;

/// Calls waiting for a reply from another node.
#[derive(Debug, Default)]
pub(crate) struct PendingCalls {
    next: Mutex<u64>,
    waiting: Mutex<HashMap<u64, (u32, ReplySender)>>,
}

impl PendingCalls {
    fn add(&self, node: u32) -> (u64, oneshot::Receiver<Result<Envelope, CallError>>) {
        let id = {
            let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
            *next += 1;
            *next
        };
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, (node, tx));
        (id, rx)
    }

    fn take(&self, id: u64) -> Option<ReplySender> {
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
            .map(|(_, tx)| tx)
    }

    /// Fail every call waiting on `node`.
    fn fail_node(&self, node: u32) {
        let lost: Vec<_> = {
            let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
            let ids: Vec<u64> = waiting
                .iter()
                .filter(|(_, (n, _))| *n == node)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| waiting.remove(&id))
                .collect()
        };
        for (_, tx) in lost {
            let _ = tx.send(Err(CallError::Noconnection));
        }
    }
}

/// Send to `node`, in the current session, if a transport is set and
/// `node` is reachable.
pub(crate) fn send(rt: &RuntimeInner, node: u32, frame: Frame) -> bool {
    if !rt.liveness.reachable(node) {
        return false;
    }
    let Some(transport) = rt.transport() else {
        return false;
    };
    let session = rt.liveness.session(node);
    transport.send(node, Packet { session, frame })
}

/// Call `to` on `node` and wait up to `timeout` for the reply.
pub(crate) async fn call(
    rt: &RuntimeInner,
    node: u32,
    from: Option<Pid>,
    to: Pid,
    msg: Envelope,
    timeout: Duration,
) -> Result<Envelope, CallError> {
    let (id, reply) = rt.pending_calls.add(node);
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    if !send(
        rt,
        node,
        Frame::Call {
            id,
            from,
            to,
            msg,
            timeout_ms,
        },
    ) {
        rt.pending_calls.take(id);
        return Err(CallError::Noconnection);
    }
    match tokio::time::timeout(timeout, reply).await {
        Ok(Ok(result)) => result,
        // The node was lost and the sender dropped.
        Ok(Err(_)) => Err(CallError::Noconnection),
        Err(_) => {
            rt.pending_calls.take(id);
            Err(CallError::Timeout)
        }
    }
}

/// Handle a packet from node `from`.
pub(crate) fn deliver(rt: &std::sync::Arc<RuntimeInner>, from_node: u32, packet: Packet) {
    match rt.liveness.on_frame(from_node, packet.session) {
        Session::Stale => return,
        Session::Current => {}
        Session::Newer(session) => ended(rt, from_node, session),
    }
    match packet.frame {
        Frame::Heartbeat { hears_you } => {
            if rt.liveness.on_heartbeat(from_node, hears_you) {
                end_session(rt, from_node);
            }
        }
        Frame::Cast { from, to, msg } => {
            let _ = delivery::cast(rt, from, to, msg);
        }
        Frame::Call {
            id,
            from,
            to,
            msg,
            timeout_ms,
        } => {
            let rt = rt.clone();
            tokio::spawn(async move {
                let timeout = Duration::from_millis(timeout_ms);
                let result = delivery::call(&rt, from, to, msg, timeout).await;
                send(&rt, from_node, Frame::Reply { id, result });
            });
        }
        Frame::Reply { id, result } => {
            if let Some(tx) = rt.pending_calls.take(id) {
                let _ = tx.send(result);
            }
        }
        Frame::Exit {
            from,
            to,
            reason,
            untrappable,
        } => delivery::exit_signal(rt, from, to, reason, untrappable),
        Frame::LinkExit { from, to, reason } => {
            // A link removed meanwhile (`unlink`) drops the signal, as in OTP.
            if rt.links.unlink(to, from)
                && let Route::Local(target) = delivery::route(rt, to)
            {
                target.exit_signal(from, reason, false);
            }
        }
        Frame::Link { from, to } => {
            let table = &rt.table;
            if !rt.links.link(from, to, |p| table.lookup(p).is_some()) {
                send(
                    rt,
                    from_node,
                    Frame::LinkExit {
                        from: to,
                        to: from,
                        reason: ExitReason::Noproc,
                    },
                );
            }
        }
        Frame::Unlink { from, to } => {
            rt.links.unlink(from, to);
        }
        Frame::Monitor {
            watcher,
            target,
            reference,
        } => {
            let table = &rt.table;
            if !rt
                .links
                .monitor_with(watcher, target, reference, |p| table.lookup(p).is_some())
            {
                send(
                    rt,
                    from_node,
                    Frame::Down {
                        reference,
                        target,
                        watcher,
                        reason: ExitReason::Noproc,
                    },
                );
            }
        }
        Frame::Demonitor { watcher, reference } => {
            rt.links.demonitor(watcher, reference);
        }
        Frame::Down {
            reference,
            target,
            watcher,
            reason,
        } => {
            // A monitor removed meanwhile (`demonitor`) drops the notice.
            if rt.links.demonitor(watcher, reference).is_some()
                && let Route::Local(handle) = delivery::route(rt, watcher)
            {
                handle.signal(Info::Down {
                    reference,
                    pid: target,
                    reason,
                });
            }
        }
    }
}

/// End the session with `node`: raise its number, so frames from the old
/// one are discarded, and end every link and monitor with it.
pub(crate) fn end_session(rt: &RuntimeInner, node: u32) {
    let session = rt.liveness.end(node);
    ended(rt, node, session);
}

/// The session with `node` ended and `session` follows: end every link and
/// monitor with it as `noconnection`, and fail calls waiting on it.
fn ended(rt: &RuntimeInner, node: u32, session: u64) {
    let down = rt.links.on_node_down(node);
    for (local, remote) in down.links {
        if let Route::Local(target) = delivery::route(rt, local) {
            target.exit_signal(remote, ExitReason::Noconnection, false);
        }
    }
    for (reference, target, watcher) in down.monitors {
        if let Route::Local(handle) = delivery::route(rt, watcher) {
            handle.signal(Info::Down {
                reference,
                pid: target,
                reason: ExitReason::Noconnection,
            });
        }
    }
    rt.pending_calls.fail_node(node);
    probe::node_down(node, session);
    rt.liveness.announce(NodeEvent::Down { node, session });
}
