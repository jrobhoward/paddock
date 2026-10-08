//! Link and monitor tables.
//!
//! The host owns both tables, so they survive guest bugs. Pids carry an
//! incarnation, so a restarted instance never inherits old links or monitors.
//!
//! Liveness is checked under the table's lock, and an exiting instance leaves
//! the process table before its links are collected. A link or monitor made
//! concurrently with an exit therefore either is seen by the exit, or sees
//! the target as dead; it is never lost.
//!
//! Entries may name pids on other nodes. Each node keeps its own side of a
//! remote link or monitor; the delivery layer keeps the two sides in step
//! and clears this node's side when the other node is lost
//! ([`LinkTable::on_node_down`]). Monitor references carry the node id in
//! their high bits, so references made by different nodes never collide in
//! one table.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::pid::Pid;

#[derive(Debug, Default)]
struct Tables {
    links: HashMap<Pid, HashSet<Pid>>,
    /// Monitors by target: reference → watcher.
    monitors: HashMap<Pid, HashMap<u64, Pid>>,
    /// Monitors by watcher: reference → target, for `demonitor` and cleanup.
    watching: HashMap<Pid, HashMap<u64, Pid>>,
}

/// Links and monitors for every instance on the node.
#[derive(Debug, Default)]
pub(crate) struct LinkTable {
    tables: Mutex<Tables>,
    next_ref: AtomicU64,
    /// This node's id, in the high bits of every reference it makes.
    node: u64,
}

/// Who must hear about an exit.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ExitNotices {
    /// Instances linked to the one that exited.
    pub(crate) linked: Vec<Pid>,
    /// Monitors of the one that exited: (reference, watcher).
    pub(crate) monitors: Vec<(u64, Pid)>,
    /// Monitors the one that exited held on others: (reference, target).
    /// A remote target's node must be told to forget them.
    pub(crate) held: Vec<(u64, Pid)>,
}

/// What losing a node means for this node's tables.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct NodeDown {
    /// Links between a local pid and a pid on the lost node: (local, remote).
    pub(crate) links: Vec<(Pid, Pid)>,
    /// Local watchers of pids on the lost node: (reference, target, watcher).
    pub(crate) monitors: Vec<(u64, Pid, Pid)>,
}

impl LinkTable {
    /// A table for node `node`.
    pub(crate) fn new(node: u32) -> Self {
        Self {
            node: u64::from(node),
            ..Self::default()
        }
    }

    fn next_reference(&self) -> u64 {
        let seq = self.next_ref.fetch_add(1, Ordering::Relaxed) + 1;
        (self.node << 40) | (seq & ((1 << 40) - 1))
    }

    /// Link `a` and `b` if `b` is alive (as judged by `alive`). Returns
    /// `false` if `b` is dead.
    pub(crate) fn link(&self, a: Pid, b: Pid, alive: impl Fn(Pid) -> bool) -> bool {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        if !alive(b) {
            return false;
        }
        t.links.entry(a).or_default().insert(b);
        t.links.entry(b).or_default().insert(a);
        true
    }

    /// Remove a link in both directions. Returns whether there was one.
    pub(crate) fn unlink(&self, a: Pid, b: Pid) -> bool {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        let mut found = false;
        for (from, to) in [(a, b), (b, a)] {
            if let Some(set) = t.links.get_mut(&from) {
                found |= set.remove(&to);
                if set.is_empty() {
                    t.links.remove(&from);
                }
            }
        }
        found
    }

    /// Have `watcher` monitor `target`. Returns the reference, and whether
    /// `target` was alive (a dead target's `down` is due at once).
    pub(crate) fn monitor(
        &self,
        watcher: Pid,
        target: Pid,
        alive: impl Fn(Pid) -> bool,
    ) -> (u64, bool) {
        let reference = self.next_reference();
        let alive = self.monitor_with(watcher, target, reference, alive);
        (reference, alive)
    }

    /// Record `watcher`'s monitor `reference` on `target` if `target` is
    /// alive, with a reference another node made. Returns whether it was.
    pub(crate) fn monitor_with(
        &self,
        watcher: Pid,
        target: Pid,
        reference: u64,
        alive: impl Fn(Pid) -> bool,
    ) -> bool {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        if !alive(target) {
            return false;
        }
        t.monitors
            .entry(target)
            .or_default()
            .insert(reference, watcher);
        t.watching
            .entry(watcher)
            .or_default()
            .insert(reference, target);
        true
    }

    /// Remove `watcher`'s monitor `reference`. Returns its target, or `None`
    /// for an unknown reference.
    pub(crate) fn demonitor(&self, watcher: Pid, reference: u64) -> Option<Pid> {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        let target = t
            .watching
            .get_mut(&watcher)
            .and_then(|m| m.remove(&reference))?;
        if t.watching.get(&watcher).is_some_and(HashMap::is_empty) {
            t.watching.remove(&watcher);
        }
        if let Some(m) = t.monitors.get_mut(&target) {
            m.remove(&reference);
            if m.is_empty() {
                t.monitors.remove(&target);
            }
        }
        Some(target)
    }

    /// Remove everything about `pid` and return who must be told it exited.
    pub(crate) fn on_exit(&self, pid: Pid) -> ExitNotices {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        let mut linked: Vec<Pid> = t
            .links
            .remove(&pid)
            .unwrap_or_default()
            .into_iter()
            .collect();
        linked.sort();
        for other in &linked {
            if let Some(set) = t.links.get_mut(other) {
                set.remove(&pid);
                if set.is_empty() {
                    t.links.remove(other);
                }
            }
        }
        let mut monitors: Vec<(u64, Pid)> = t
            .monitors
            .remove(&pid)
            .unwrap_or_default()
            .into_iter()
            .collect();
        monitors.sort();
        for (reference, watcher) in &monitors {
            if let Some(m) = t.watching.get_mut(watcher) {
                m.remove(reference);
            }
        }
        // Monitors this instance held on others die with it.
        let mut held: Vec<(u64, Pid)> = t
            .watching
            .remove(&pid)
            .unwrap_or_default()
            .into_iter()
            .collect();
        held.sort();
        for (reference, target) in &held {
            if let Some(m) = t.monitors.get_mut(target) {
                m.remove(reference);
                if m.is_empty() {
                    t.monitors.remove(target);
                }
            }
        }
        ExitNotices {
            linked,
            monitors,
            held,
        }
    }

    /// Remove every entry between this node and `node`, and return what the
    /// local side must be told (`noconnection`).
    pub(crate) fn on_node_down(&self, node: u32) -> NodeDown {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        let mut down = NodeDown::default();
        let locals: Vec<Pid> = t.links.keys().copied().filter(|p| p.node != node).collect();
        for local in locals {
            let Some(set) = t.links.get_mut(&local) else {
                continue;
            };
            let remote: Vec<Pid> = set.iter().copied().filter(|p| p.node == node).collect();
            for r in &remote {
                set.remove(r);
                down.links.push((local, *r));
            }
            if set.is_empty() {
                t.links.remove(&local);
            }
        }
        t.links.retain(|p, _| p.node != node);
        // Local watchers of remote targets.
        let watchers: Vec<Pid> = t
            .watching
            .keys()
            .copied()
            .filter(|p| p.node != node)
            .collect();
        for watcher in watchers {
            let Some(m) = t.watching.get_mut(&watcher) else {
                continue;
            };
            let lost: Vec<(u64, Pid)> = m
                .iter()
                .filter(|(_, target)| target.node == node)
                .map(|(r, target)| (*r, *target))
                .collect();
            for (reference, target) in lost {
                m.remove(&reference);
                down.monitors.push((reference, target, watcher));
            }
            if m.is_empty() {
                t.watching.remove(&watcher);
            }
        }
        // Remote watchers of local targets: forget them.
        t.watching.retain(|p, _| p.node != node);
        for m in t.monitors.values_mut() {
            m.retain(|_, watcher| watcher.node != node);
        }
        t.monitors
            .retain(|target, m| target.node != node && !m.is_empty());
        down.links.sort();
        down.monitors.sort();
        down
    }
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod links_tests;
