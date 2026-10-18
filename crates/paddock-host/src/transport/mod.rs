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

mod lane;
pub mod liveness;
pub mod membership;
pub mod memory;
pub mod quic;
pub mod wire;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::acl::{self, Stamp};
use crate::delivery::{self, Origin, Route};
use crate::links::LinkFrom;
use crate::message::{CallError, Envelope, ExitReason, Info};
use crate::pid::Pid;
use crate::probe;
use crate::runtime::RuntimeInner;
pub use lane::{LaneStats, SendError};
use liveness::{NodeEvent, Session};
use membership::Member;

/// A frame and the session it belongs to ([`liveness`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Packet {
    /// The sender's creation ([`membership`]).
    pub creation: u32,
    /// The receiver's creation as the sender knows it; 0 if not yet known.
    /// A frame meant for an earlier creation is discarded.
    pub to_creation: u32,
    /// The sender's session number with the receiving node.
    pub session: u64,
    /// The frame.
    pub frame: Frame,
}

/// One message between nodes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum Frame {
    /// The sender is alive ([`liveness`]).
    Heartbeat {
        /// Whether the sender hears the receiving node.
        hears_you: bool,
        /// What the sender's beats carry ([`liveness`]).
        #[serde(with = "wire::bytes")]
        payload: Vec<u8>,
    },
    /// The sender joins the cluster ([`membership`]); the receiver answers
    /// with [`Frame::Members`].
    Join {
        /// The sender.
        member: Member,
    },
    /// Members the sender knows.
    Members {
        /// The members, the sender included.
        members: Vec<Member>,
    },
    /// The sender leaves the cluster.
    Leave,
    /// Between a node of an HA pair and its witness ([`crate::failover`]).
    Failover(#[serde(with = "wire::failover")] crate::failover::FailoverMsg),
    /// A cast to `to`.
    Cast {
        /// The sender, if an instance.
        from: Option<Pid>,
        /// The sender's app and routes, if an instance ([`crate::acl`]).
        sender: Option<Stamp>,
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
        /// The caller's app and routes, if an instance ([`crate::acl`]).
        sender: Option<Stamp>,
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
        /// The sender's app and routes ([`crate::acl`]).
        sender: Option<Stamp>,
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
        /// Its app and routes ([`crate::acl`]).
        sender: Option<Stamp>,
        /// The one it linked to.
        to: Pid,
    },
    /// Remove the link between `from` and `to`; the receiver answers with
    /// [`Frame::UnlinkAck`]. Until then the sender ignores a [`Frame::Link`]
    /// from `to`, sent before `to` saw this frame (OTP 23's unlink protocol;
    /// spike S12).
    Unlink {
        /// The instance that unlinked.
        from: Pid,
        /// Its partner.
        to: Pid,
        /// Names this unlink in its acknowledgement.
        id: u64,
    },
    /// The receiver of [`Frame::Unlink`] `id` removed its side of the link.
    UnlinkAck {
        /// The partner that was unlinked.
        from: Pid,
        /// The instance that unlinked, on the receiving node.
        to: Pid,
        /// The unlink's id.
        id: u64,
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
    /// Every member the sender has in each process group, replacing what
    /// the receiver knew of the sender's members (ROADMAP D4).
    GroupSync {
        /// Each group and the sender's members in it.
        groups: Vec<(String, Vec<Pid>)>,
    },
    /// `pid`, on the sender, joined `group`.
    GroupJoin {
        /// The group.
        group: String,
        /// The member.
        pid: Pid,
    },
    /// `pid`, on the sender, left `group` (or exited).
    GroupLeave {
        /// The group.
        group: String,
        /// The member.
        pid: Pid,
    },
    /// A publish to `group`, for the receiver's members of it.
    GroupPublish {
        /// The group.
        group: String,
        /// The publisher.
        from: Pid,
        /// The publisher's app and routes ([`crate::acl`]).
        sender: Option<Stamp>,
        /// The message.
        msg: Envelope,
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
    /// Replication between the nodes of an HA pair (ROADMAP K2): log
    /// entries, acknowledgements, snapshots, and operations the standby
    /// forwards to the primary, as CBOR the replication module reads.
    Replication {
        /// The message.
        #[serde(with = "wire::bytes")]
        body: Vec<u8>,
    },
    /// A call from the sending node's host to the app called `app` on the
    /// receiving node, whose pool picks the instance as for a local caller
    /// (an HTTP request forwarded from a standby's ingress, ROADMAP D11);
    /// answered with [`Frame::Reply`].
    AppCall {
        /// The caller's node's id for this call.
        id: u64,
        /// The app.
        app: String,
        /// The message.
        msg: Envelope,
        /// How long the caller waits.
        timeout_ms: u64,
    },
}

/// Carries packets to other nodes.
pub trait Transport: Send + Sync + 'static {
    /// Send `packet` to `node`.
    ///
    /// # Errors
    ///
    /// Why the packet was dropped: `node` cannot be reached now, or its
    /// link is congested ([`SendError::Busy`] for a cast, call or reply
    /// past the message budget; [`SendError::Full`] past the frame cap).
    fn send(&self, node: u32, packet: Packet) -> Result<(), SendError>;
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

/// Calls from other nodes being handled, per node, against a limit per
/// node (`incoming_calls_per_peer`; 0 for none).
#[derive(Debug)]
pub(crate) struct IncomingCalls {
    max: u32,
    running: Mutex<HashMap<u32, Running>>,
}

#[derive(Debug, Default)]
struct Running {
    count: u32,
    /// A call was refused since the count was last at half the limit or
    /// below, so the warning has been logged; a peer held at the limit is
    /// not logged once per call.
    refusing: bool,
}

/// Why a call from another node was not started.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The first refusal since the node was last at half the limit or
    /// below.
    First,
    /// The node was already being refused.
    Again,
}

impl IncomingCalls {
    pub(crate) fn new(max: u32) -> Self {
        Self {
            max,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// Count a call from `node` as started, unless `max` are running.
    pub(crate) fn start(&self, node: u32) -> Result<(), Refused> {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = running.entry(node).or_default();
        if self.max != 0 && entry.count >= self.max {
            return Err(if std::mem::replace(&mut entry.refusing, true) {
                Refused::Again
            } else {
                Refused::First
            });
        }
        entry.count += 1;
        Ok(())
    }

    /// Count a call from `node` as finished.
    pub(crate) fn finish(&self, node: u32) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = running.get_mut(&node) {
            entry.count = entry.count.saturating_sub(1);
            if entry.count <= self.max / 2 {
                entry.refusing = false;
            }
            if entry.count == 0 {
                running.remove(&node);
            }
        }
    }

    #[cfg(test)]
    fn running(&self, node: u32) -> u32 {
        let running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.get(&node).map_or(0, |r| r.count)
    }
}

/// A call from another node counted in [`IncomingCalls`] until dropped, so
/// a task that ends any way releases its place.
struct IncomingCall {
    rt: Arc<RuntimeInner>,
    node: u32,
}

impl Drop for IncomingCall {
    fn drop(&mut self) {
        self.rt.incoming_calls.finish(self.node);
    }
}

/// Handle a call from `from_node`: run it in a task of its own and send the
/// reply, or answer `overloaded` at once if that node has as many calls
/// running as it may.
/// What an incoming call is addressed to.
enum CallTo {
    /// An instance, from the sending node's `from` (its host if `None`).
    Pid { from: Option<Pid>, to: Pid },
    /// An app's pool, from the sending node's host.
    App(String),
}

fn incoming_call(
    rt: &Arc<RuntimeInner>,
    from_node: u32,
    id: u64,
    to: CallTo,
    msg: Envelope,
    timeout_ms: u64,
) {
    match rt.incoming_calls.start(from_node) {
        Ok(()) => {
            let slot = IncomingCall {
                rt: rt.clone(),
                node: from_node,
            };
            tokio::spawn(async move {
                let rt = &slot.rt;
                let timeout = Duration::from_millis(timeout_ms);
                let result = match to {
                    CallTo::Pid { from, to } => {
                        let origin = from.map_or(Origin::Host, Origin::Remote);
                        delivery::call(rt, origin, to, msg, timeout).await
                    }
                    CallTo::App(app) => match rt.route_app(&app, &msg) {
                        Some(target) => target.call(msg, timeout).await,
                        None => Err(CallError::Noproc),
                    },
                };
                send(rt, from_node, Frame::Reply { id, result });
            });
        }
        Err(refused) => {
            if refused == Refused::First {
                tracing::warn!(
                    node = from_node,
                    limit = rt.incoming_calls.max,
                    "calls from a node at their limit; answering overloaded"
                );
            }
            send(
                rt,
                from_node,
                Frame::Reply {
                    id,
                    result: Err(CallError::Overloaded),
                },
            );
        }
    }
}

/// Send a notice to `node`: an exit, a down, an unlink or demonitor, or an
/// answer to one. A notice that cannot be sent ends the session with
/// `node`, so the other side drops what the notice was about with
/// `noconnection` instead of keeping it forever (spike S12, R1).
pub(crate) fn send_notice(rt: &RuntimeInner, node: u32, frame: Frame) {
    if let Err(why) = try_send(rt, node, frame)
        && rt.liveness.watches(node)
    {
        tracing::debug!(node, %why, "a notice could not be sent; ending the session");
        end_session(rt, node);
    }
}

/// Whether links and monitors with `node`'s pids may be made: a frame from
/// its current start has been heard. Before that, its session numbering is
/// adopted on first contact without ending anything, which is safe only
/// while nothing exists with it yet (spike S12, R4).
pub(crate) fn contacted(rt: &RuntimeInner, node: u32) -> bool {
    rt.liveness.creation(node) != 0
}

/// Whether `frame` is control traffic (heartbeats, membership, failover):
/// on its own stream, outside the data lane's budget ([`lane`]).
pub(crate) fn control(frame: &Frame) -> bool {
    matches!(
        frame,
        Frame::Heartbeat { .. }
            | Frame::Join { .. }
            | Frame::Members { .. }
            | Frame::Leave
            | Frame::Failover(_)
    )
}

/// What a caller or caster is told when its message was not sent: the rule
/// for a full mailbox for a congested link (`overloaded`), `noconnection`
/// otherwise.
pub(crate) fn refused(why: SendError) -> CallError {
    match why {
        SendError::Busy | SendError::Full => CallError::Overloaded,
        SendError::Unreachable => CallError::Noconnection,
    }
}

/// Send to `node`, in the current session; whether it was sent.
pub(crate) fn send(rt: &RuntimeInner, node: u32, frame: Frame) -> bool {
    try_send(rt, node, frame).is_ok()
}

/// Send to `node`, in the current session, if a transport is set and
/// `node` is reachable.
pub(crate) fn try_send(rt: &RuntimeInner, node: u32, frame: Frame) -> Result<(), SendError> {
    if !rt.liveness.reachable(node) {
        return Err(SendError::Unreachable);
    }
    let Some(transport) = rt.transport() else {
        return Err(SendError::Unreachable);
    };
    let packet = Packet {
        creation: rt.creation(),
        to_creation: rt.liveness.creation(node),
        session: rt.liveness.session(node),
        frame,
    };
    transport.send(node, packet)
}

/// Call `to` on `node` and wait up to `timeout` for the reply.
pub(crate) async fn call(
    rt: &RuntimeInner,
    node: u32,
    from: Option<Pid>,
    sender: Option<Stamp>,
    to: Pid,
    msg: Envelope,
    timeout: Duration,
) -> Result<Envelope, CallError> {
    send_call(rt, node, timeout, |id, timeout_ms| Frame::Call {
        id,
        from,
        sender,
        to,
        msg,
        timeout_ms,
    })
    .await
}

/// Call the app called `app` on `node`, from this node's host; its pool
/// picks the instance.
pub(crate) async fn call_app(
    rt: &RuntimeInner,
    node: u32,
    app: String,
    msg: Envelope,
    timeout: Duration,
) -> Result<Envelope, CallError> {
    send_call(rt, node, timeout, |id, timeout_ms| Frame::AppCall {
        id,
        app,
        msg,
        timeout_ms,
    })
    .await
}

/// Send the call `frame(id, timeout_ms)` makes and wait up to `timeout`
/// for its [`Frame::Reply`].
async fn send_call(
    rt: &RuntimeInner,
    node: u32,
    timeout: Duration,
    frame: impl FnOnce(u64, u64) -> Frame,
) -> Result<Envelope, CallError> {
    let (id, reply) = rt.pending_calls.add(node);
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    if let Err(why) = try_send(rt, node, frame(id, timeout_ms)) {
        rt.pending_calls.take(id);
        return Err(refused(why));
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

/// Whether a call, cast, exit signal or link from another node's `from`,
/// with the sending node's `stamp`, may reach the local `to`
/// ([`crate::acl`]). The sending node's host (no `from`) may. An instance's
/// frame without a stamp may not. A `to` that is not a live local instance
/// is left to the usual `noproc` handling.
fn admits(rt: &RuntimeInner, from: Option<Pid>, stamp: Option<&Stamp>, to: Pid) -> bool {
    if from.is_none() {
        return true;
    }
    let Route::Local(target) = delivery::route(rt, to) else {
        return true;
    };
    let ok = stamp.is_some_and(|s| {
        acl::allowed(&s.app, &s.may_call, target.app(), &target.acl().accept_from)
    });
    if !ok {
        tracing::debug!(?from, %to, "refused by the messaging ACL");
    }
    ok
}

/// Whether every pid in `frame` that names its sender belongs to
/// `from_node`: a node speaks only for its own instances.
fn sender_on(frame: &Frame, from_node: u32) -> bool {
    let sender = match frame {
        Frame::Cast { from, .. } | Frame::Call { from, .. } => *from,
        Frame::Exit { from, .. }
        | Frame::LinkExit { from, .. }
        | Frame::Link { from, .. }
        | Frame::Unlink { from, .. }
        | Frame::UnlinkAck { from, .. } => Some(*from),
        Frame::Monitor { watcher, .. } | Frame::Demonitor { watcher, .. } => Some(*watcher),
        Frame::GroupJoin { pid, .. } | Frame::GroupLeave { pid, .. } => Some(*pid),
        Frame::GroupPublish { from, .. } => Some(*from),
        Frame::Down { target, .. } => Some(*target),
        _ => None,
    };
    sender.is_none_or(|p| p.node == from_node)
}

/// Handle a packet from node `from`.
pub(crate) fn deliver(rt: &std::sync::Arc<RuntimeInner>, from_node: u32, packet: Packet) {
    if packet.to_creation != 0 && packet.to_creation != rt.creation() {
        // Meant for an earlier start of this node.
        return;
    }
    if !sender_on(&packet.frame, from_node) {
        tracing::warn!(
            node = from_node,
            "a frame naming another node's instance as its sender; dropped"
        );
        return;
    }
    match rt
        .liveness
        .on_frame(from_node, packet.creation, packet.session)
    {
        Session::Stale => {
            // A heartbeat says what its sender is now, whatever the session.
            if let Frame::Heartbeat { hears_you, payload } = packet.frame
                && rt.liveness.on_heartbeat(from_node, hears_you, payload)
            {
                end_session(rt, from_node);
            }
            return;
        }
        Session::Current => {}
        Session::Newer(session) => ended(rt, from_node, session),
    }
    match packet.frame {
        Frame::Heartbeat { hears_you, payload } => {
            if rt.liveness.on_heartbeat(from_node, hears_you, payload) {
                end_session(rt, from_node);
            }
        }
        Frame::Join { member } => {
            let new = rt.membership.merge(std::slice::from_ref(&member));
            send(
                rt,
                from_node,
                Frame::Members {
                    members: rt.membership.list(),
                },
            );
            if !new.is_empty() {
                for other in rt.membership.others() {
                    if other != from_node {
                        send(
                            rt,
                            other,
                            Frame::Members {
                                members: new.clone(),
                            },
                        );
                    }
                }
            }
            welcome(rt, &new, None);
        }
        Frame::Members { members } => {
            let new = rt.membership.merge(&members);
            welcome(rt, &new, Some(from_node));
        }
        Frame::Failover(msg) => rt.failover.deliver(rt, from_node, msg),
        Frame::Replication { body } => crate::replication::deliver(rt, from_node, &body),
        Frame::Leave => {
            if rt.membership.remove(from_node) {
                end_session(rt, from_node);
                rt.liveness.remove_peer(from_node);
                rt.liveness.announce(NodeEvent::Left { node: from_node });
            }
        }
        Frame::Cast {
            from,
            sender,
            to,
            msg,
        } => {
            if admits(rt, from, sender.as_ref(), to) {
                let _ = delivery::cast(rt, from.map_or(Origin::Host, Origin::Remote), to, msg);
            }
        }
        Frame::Call {
            id,
            from,
            sender,
            to,
            msg,
            timeout_ms,
        } => {
            if admits(rt, from, sender.as_ref(), to) {
                incoming_call(rt, from_node, id, CallTo::Pid { from, to }, msg, timeout_ms);
            } else {
                send(
                    rt,
                    from_node,
                    Frame::Reply {
                        id,
                        result: Err(CallError::AccessDenied),
                    },
                );
            }
        }
        Frame::Reply { id, result } => {
            if let Some(tx) = rt.pending_calls.take(id) {
                let _ = tx.send(result);
            }
        }
        Frame::AppCall {
            id,
            app,
            msg,
            timeout_ms,
        } => incoming_call(rt, from_node, id, CallTo::App(app), msg, timeout_ms),
        Frame::Exit {
            from,
            sender,
            to,
            reason,
            untrappable,
        } => {
            if admits(rt, Some(from), sender.as_ref(), to) {
                delivery::exit_signal(rt, Origin::Remote(from), to, reason, untrappable);
            }
        }
        Frame::LinkExit { from, to, reason } => {
            // A link removed meanwhile (`unlink`) drops the signal, as in OTP.
            if rt.links.unlink(to, from)
                && let Route::Local(target) = delivery::route(rt, to)
            {
                target.exit_signal(from, reason, false);
            }
        }
        Frame::Link { from, sender, to } => {
            let table = &rt.table;
            // A link the ACL refuses is answered as one to a dead pid.
            let admitted = admits(rt, Some(from), sender.as_ref(), to);
            let alive = |p: Pid| admitted && table.lookup(p).is_some();
            if rt.links.link_from(to, from, alive) == LinkFrom::Dead {
                send_notice(
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
        Frame::Unlink { from, to, id } => {
            rt.links.unlink(from, to);
            send_notice(
                rt,
                from_node,
                Frame::UnlinkAck {
                    from: to,
                    to: from,
                    id,
                },
            );
        }
        Frame::UnlinkAck { from, to, id } => rt.links.unlinked(to, from, id),
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
                send_notice(
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
        Frame::GroupSync { groups } => rt.services.groups().synced_from(from_node, groups),
        Frame::GroupJoin { group, pid } => rt.services.groups().joined_on(from_node, group, pid),
        Frame::GroupLeave { group, pid } => rt.services.groups().left_on(from_node, &group, pid),
        Frame::GroupPublish {
            group,
            from,
            sender,
            msg,
        } => rt
            .services
            .groups()
            .published_from(&group, from, sender.as_ref(), &msg),
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

/// Members just learned: watch each, announce it, and, except to `told_by`
/// (who knows this node already), introduce this node.
fn welcome(rt: &std::sync::Arc<RuntimeInner>, new: &[Member], told_by: Option<u32>) {
    for m in new {
        rt.liveness.add_peer(rt, m.node);
        rt.liveness.announce(NodeEvent::Joined {
            node: m.node,
            creation: m.creation,
        });
        if Some(m.node) != told_by {
            send(
                rt,
                m.node,
                Frame::Join {
                    member: rt.membership.me(),
                },
            );
        }
    }
}

/// Join the cluster through `seeds`.
pub(crate) fn join(rt: &std::sync::Arc<RuntimeInner>, seeds: &[u32]) {
    let me = rt.membership.me();
    for &seed in seeds.iter().filter(|&&s| s != rt.node()) {
        rt.liveness.add_peer(rt, seed);
        send(rt, seed, Frame::Join { member: me.clone() });
    }
}

/// Leave the cluster: tell every member, end every session, and stop
/// watching them.
pub(crate) fn leave(rt: &RuntimeInner) {
    for node in rt.membership.others() {
        send(rt, node, Frame::Leave);
        rt.membership.remove(node);
        end_session(rt, node);
        rt.liveness.remove_peer(node);
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
    rt.services.groups().forget(node);
    probe::node_down(node, session);
    rt.liveness.announce(NodeEvent::Down { node, session });
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod transport_tests;
