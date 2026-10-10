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
//!   monitors fire at once with `noconnection`.
//!
//! These are Erlang's rules for unreachable nodes (DESIGN.md, "Failure
//! handling").

use std::time::Duration;

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
    from: Option<Pid>,
    to: Pid,
    msg: Envelope,
) -> Result<(), CallError> {
    match route(rt, to) {
        Route::Local(target) => target.cast_from(from, msg),
        Route::Dead => Err(CallError::Noproc),
        Route::Remote(node) => {
            if transport::send(rt, node, Frame::Cast { from, to, msg }) {
                Ok(())
            } else {
                Err(CallError::Noconnection)
            }
        }
    }
}

/// Send a call and wait up to `timeout` for the reply.
pub(crate) async fn call(
    rt: &RuntimeInner,
    from: Option<Pid>,
    to: Pid,
    msg: Envelope,
    timeout: Duration,
) -> Result<Envelope, CallError> {
    match route(rt, to) {
        Route::Local(target) => target.call_from(from, msg, timeout).await,
        Route::Dead => Err(CallError::Noproc),
        Route::Remote(node) => transport::call(rt, node, from, to, msg, timeout).await,
    }
}

/// Send an exit signal from `from` to `to`. A dead or unreachable target
/// drops it.
pub(crate) fn exit_signal(
    rt: &RuntimeInner,
    from: Pid,
    to: Pid,
    reason: ExitReason,
    untrappable: bool,
) {
    match route(rt, to) {
        Route::Local(target) => target.exit_signal(from, reason, untrappable),
        Route::Dead => {}
        Route::Remote(node) => {
            transport::send(
                rt,
                node,
                Frame::Exit {
                    from,
                    to,
                    reason,
                    untrappable,
                },
            );
        }
    }
}

/// Link local `a` with `b`. `Err` carries the exit reason `a` must receive
/// at once because `b` cannot be linked: `noproc` for a dead pid,
/// `noconnection` for an unreachable node. A remote `b` that turns out to be
/// dead answers later with a `noproc` link exit.
pub(crate) fn link(rt: &RuntimeInner, a: Pid, b: Pid) -> Result<(), ExitReason> {
    if let Route::Remote(node) = route(rt, b) {
        rt.links.link(a, b, |_| true);
        if transport::send(rt, node, Frame::Link { from: a, to: b }) {
            return Ok(());
        }
        rt.links.unlink(a, b);
        return Err(ExitReason::Noconnection);
    }
    let table = &rt.table;
    if rt.links.link(a, b, |p| table.lookup(p).is_some()) {
        Ok(())
    } else {
        Err(ExitReason::Noproc)
    }
}

/// Remove the link between local `a` and `b`, if any, on both nodes.
pub(crate) fn unlink(rt: &RuntimeInner, a: Pid, b: Pid) {
    rt.links.unlink(a, b);
    if let Route::Remote(node) = route(rt, b) {
        transport::send(rt, node, Frame::Unlink { from: a, to: b });
    }
}

/// Have local `watcher` monitor `target`. Returns the reference, and the
/// reason of the `down` due at once if `target` cannot be monitored. A
/// remote `target` that turns out to be dead answers later with a `noproc`
/// down.
pub(crate) fn monitor(rt: &RuntimeInner, watcher: Pid, target: Pid) -> (u64, Option<ExitReason>) {
    if let Route::Remote(node) = route(rt, target) {
        let (reference, _) = rt.links.monitor(watcher, target, |_| true);
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
        transport::send(rt, node, Frame::Demonitor { watcher, reference });
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
                transport::send(
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
                transport::send(
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
            transport::send(
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
