//! Where messages, exit signals, links and monitors for a pid go.
//!
//! Every operation of the process core that names a pid goes through here,
//! so routing is decided in one place ([`route`]):
//!
//! - a live local pid gets the operation directly;
//! - a dead local pid (exited, never existed, or an earlier incarnation)
//!   gets `noproc` semantics;
//! - a pid on another node goes to that node through the runtime's
//!   [`Transport`](crate::transport::Transport). With no transport, or a
//!   node it cannot reach, the pid gets `noconnection` semantics: calls fail
//!   with `noconnection`, casts and exit signals are dropped, and links and
//!   monitors fire at once with `noconnection`. Links and monitors also
//!   fire at once with `noconnection` until a frame from the node's current
//!   start has been heard.
//!
//! Calls, casts, exit signals and links pass the messaging ACLs
//! ([`crate::acl`]) here when both ends are on this node; one from another
//! node was checked against its sender's stamp in `transport::deliver`.
//!
//! Exit and down notices, unlinks and demonitors that cannot be sent end
//! the session with their node, so both nodes drop the entries they were
//! about ([`transport::send_notice`]; spike S12).
//!
//! These are Erlang's rules for unreachable nodes (DESIGN.md, "Failure
//! handling").

use std::time::Duration;

use crate::acl::{self, Stamp};
use crate::instance::InstanceHandle;
use crate::message::{CallError, Envelope, ExitReason, Info};
use crate::pid::Pid;
use crate::runtime::RuntimeInner;
use crate::transport::{self, Frame};

/// Where a pid lives, as seen from this node.
pub(crate) enum Route {
    /// A live instance on this node.
    Local(InstanceHandle),
    /// On this node, but not alive: exited, never existed, or an earlier
    /// incarnation.
    Dead,
    /// On another node.
    Remote(u32),
}

/// Who sends a message, exit signal or link.
#[derive(Clone, Copy)]
pub(crate) enum Origin<'a> {
    /// The host: the admin API, the HTTP ingress, a health check, an
    /// extension.
    Host,
    /// An instance of this node, connection handlers included (they are
    /// not in the process table).
    Local(&'a InstanceHandle),
    /// An instance on another node; its frame passed the ACL on arrival.
    Remote(Pid),
}

impl Origin<'_> {
    /// The sender's pid, if an instance.
    pub(crate) fn pid(self) -> Option<Pid> {
        match self {
            Self::Host => None,
            Self::Local(h) => Some(h.pid()),
            Self::Remote(p) => Some(p),
        }
    }
}

/// Whether `from` may send to the local instance `target` ([`crate::acl`]).
/// The host may; an instance on another node was checked on arrival.
fn permits(from: Origin<'_>, target: &InstanceHandle) -> bool {
    let Origin::Local(sender) = from else {
        return true;
    };
    let ok = acl::allowed(
        sender.app(),
        &sender.acl().may_call,
        target.app(),
        &target.acl().accept_from,
    );
    if !ok {
        tracing::debug!(from = %sender.pid(), to = %target.pid(), "refused by the messaging ACL");
    }
    ok
}

/// What a frame to another node says about its sender, an instance of
/// this node: its app and routes.
pub(crate) fn stamp(from: Origin<'_>) -> Option<Stamp> {
    let Origin::Local(sender) = from else {
        return None;
    };
    Some(Stamp {
        app: sender.app().to_string(),
        may_call: sender.acl().may_call.clone(),
    })
}

/// Where `pid` lives.
pub(crate) fn route(rt: &RuntimeInner, pid: Pid) -> Route {
    if pid.node != rt.node() {
        return Route::Remote(pid.node);
    }
    rt.table.lookup(pid).map_or(Route::Dead, Route::Local)
}

/// Send a cast. At most once: the error says why it was not queued, and
/// senders that are instances drop it silently. A cast to another node is
/// `Ok` once handed to the transport.
pub(crate) fn cast(
    rt: &RuntimeInner,
    from: Origin<'_>,
    to: Pid,
    msg: Envelope,
) -> Result<(), CallError> {
    match route(rt, to) {
        Route::Local(target) if !permits(from, &target) => Err(CallError::AccessDenied),
        Route::Local(target) => target.cast_from(from.pid(), msg),
        Route::Dead => Err(CallError::Noproc),
        Route::Remote(node) => transport::try_send(
            rt,
            node,
            Frame::Cast {
                from: from.pid(),
                sender: stamp(from),
                to,
                msg,
            },
        )
        .map_err(transport::refused),
    }
}

/// Send a call and wait up to `timeout` for the reply.
pub(crate) async fn call(
    rt: &RuntimeInner,
    from: Origin<'_>,
    to: Pid,
    msg: Envelope,
    timeout: Duration,
) -> Result<Envelope, CallError> {
    match route(rt, to) {
        Route::Local(target) if !permits(from, &target) => Err(CallError::AccessDenied),
        Route::Local(target) => target.call_from(from.pid(), msg, timeout).await,
        Route::Dead => Err(CallError::Noproc),
        Route::Remote(node) => {
            let sender = stamp(from);
            transport::call(rt, node, from.pid(), sender, to, msg, timeout).await
        }
    }
}

/// Send an exit signal from instance `from` to `to`. A dead or unreachable
/// target, or one the ACL refuses, drops it.
pub(crate) fn exit_signal(
    rt: &RuntimeInner,
    from: Origin<'_>,
    to: Pid,
    reason: ExitReason,
    untrappable: bool,
) {
    let Some(pid) = from.pid() else {
        return;
    };
    match route(rt, to) {
        Route::Local(target) if !permits(from, &target) => {}
        Route::Local(target) => target.exit_signal(pid, reason, untrappable),
        Route::Dead => {}
        Route::Remote(node) => {
            transport::send(
                rt,
                node,
                Frame::Exit {
                    from: pid,
                    sender: stamp(from),
                    to,
                    reason,
                    untrappable,
                },
            );
        }
    }
}

/// Link local instance `me` with `b`. `Err` carries the exit reason `me`
/// must receive at once because `b` cannot be linked: `noproc` for a dead
/// pid or one the ACL refuses, `noconnection` for a node that is
/// unreachable or not yet heard from. A remote `b` that turns out to be
/// dead, or refuses, answers later with a `noproc` link exit.
pub(crate) fn link(rt: &RuntimeInner, me: &InstanceHandle, b: Pid) -> Result<(), ExitReason> {
    let a = me.pid();
    match route(rt, b) {
        Route::Remote(node) => {
            if !transport::contacted(rt, node) {
                return Err(ExitReason::Noconnection);
            }
            rt.links.link(a, b, |_| true);
            let sent = transport::send(
                rt,
                node,
                Frame::Link {
                    from: a,
                    sender: stamp(Origin::Local(me)),
                    to: b,
                },
            );
            if sent {
                return Ok(());
            }
            rt.links.unlink(a, b);
            Err(ExitReason::Noconnection)
        }
        // A link the ACL refuses looks like a link to a dead pid.
        Route::Local(target) if !permits(Origin::Local(me), &target) => Err(ExitReason::Noproc),
        _ => {
            let table = &rt.table;
            if rt.links.link(a, b, |p| table.lookup(p).is_some()) {
                Ok(())
            } else {
                Err(ExitReason::Noproc)
            }
        }
    }
}

/// Remove the link between local `a` and `b`, if any, on both nodes. A
/// remote unlink is acknowledged ([`crate::links::LinkTable::unlink_remote`]).
pub(crate) fn unlink(rt: &RuntimeInner, a: Pid, b: Pid) {
    if let Route::Remote(node) = route(rt, b) {
        if let Some(id) = rt.links.unlink_remote(a, b) {
            transport::send_notice(rt, node, Frame::Unlink { from: a, to: b, id });
        }
        return;
    }
    rt.links.unlink(a, b);
}

/// Have local `watcher` monitor `target`. Returns the reference, and the
/// reason of the `down` due at once if `target` cannot be monitored. A
/// remote `target` that turns out to be dead answers later with a `noproc`
/// down.
pub(crate) fn monitor(rt: &RuntimeInner, watcher: Pid, target: Pid) -> (u64, Option<ExitReason>) {
    if let Route::Remote(node) = route(rt, target) {
        let (reference, _) = rt.links.monitor(watcher, target, |_| true);
        if !transport::contacted(rt, node) {
            rt.links.demonitor(watcher, reference);
            return (reference, Some(ExitReason::Noconnection));
        }
        let sent = transport::send(
            rt,
            node,
            Frame::Monitor {
                watcher,
                target,
                reference,
            },
        );
        if sent {
            return (reference, None);
        }
        rt.links.demonitor(watcher, reference);
        return (reference, Some(ExitReason::Noconnection));
    }
    let table = &rt.table;
    let (reference, alive) = rt
        .links
        .monitor(watcher, target, |p| table.lookup(p).is_some());
    (reference, (!alive).then_some(ExitReason::Noproc))
}

/// Remove local `watcher`'s monitor `reference`, on both nodes.
pub(crate) fn demonitor(rt: &RuntimeInner, watcher: Pid, reference: u64) {
    if let Some(target) = rt.links.demonitor(watcher, reference)
        && let Route::Remote(node) = route(rt, target)
    {
        transport::send_notice(rt, node, Frame::Demonitor { watcher, reference });
    }
}

/// Tell `pid`'s linked instances and monitors that it exited, on any node,
/// and drop the monitors it held on other nodes.
pub(crate) fn notify_exit(rt: &RuntimeInner, pid: Pid, reason: &ExitReason) {
    rt.services.exited(pid);
    let notices = rt.links.on_exit(pid);
    for other in notices.linked {
        match route(rt, other) {
            Route::Local(target) => target.exit_signal(pid, reason.clone(), false),
            Route::Dead => {}
            Route::Remote(node) => {
                transport::send_notice(
                    rt,
                    node,
                    Frame::LinkExit {
                        from: pid,
                        to: other,
                        reason: reason.clone(),
                    },
                );
            }
        }
    }
    for (reference, watcher) in notices.monitors {
        match route(rt, watcher) {
            Route::Local(handle) => handle.signal(Info::Down {
                reference,
                pid,
                reason: reason.clone(),
            }),
            Route::Dead => {}
            Route::Remote(node) => {
                transport::send_notice(
                    rt,
                    node,
                    Frame::Down {
                        reference,
                        target: pid,
                        watcher,
                        reason: reason.clone(),
                    },
                );
            }
        }
    }
    for (reference, target) in notices.held {
        if let Route::Remote(node) = route(rt, target) {
            transport::send_notice(
                rt,
                node,
                Frame::Demonitor {
                    watcher: pid,
                    reference,
                },
            );
        }
    }
}

/// The pid registered under `name` on this node, or one instance of the
/// running app called `name` (round-robin).
pub(crate) fn whereis(rt: &RuntimeInner, name: &str) -> Option<Pid> {
    rt.registry
        .whereis(name)
        .or_else(|| rt.pool_member(name).map(|h| h.pid()))
}
