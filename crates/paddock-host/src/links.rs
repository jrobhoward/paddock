//! Link and monitor tables.
//!
//! The host owns both tables, so they survive guest bugs. Pids carry an
//! incarnation, so a restarted instance never inherits old links or monitors.
//!
//! Liveness is checked under the table's lock, and an exiting instance leaves
//! the process table before its links are collected. A link or monitor made
//! concurrently with an exit therefore either is seen by the exit, or sees
//! the target as dead; it is never lost.

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
}

/// Who must hear about an exit.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ExitNotices {
    /// Instances linked to the one that exited.
    pub(crate) linked: Vec<Pid>,
    /// Monitors of the one that exited: (reference, watcher).
    pub(crate) monitors: Vec<(u64, Pid)>,
}

impl LinkTable {
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

    /// Remove a link in both directions.
    pub(crate) fn unlink(&self, a: Pid, b: Pid) {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        for (from, to) in [(a, b), (b, a)] {
            if let Some(set) = t.links.get_mut(&from) {
                set.remove(&to);
                if set.is_empty() {
                    t.links.remove(&from);
                }
            }
        }
    }

    /// Have `watcher` monitor `target`. Returns the reference, and whether
    /// `target` was alive (a dead target's `down` is due at once).
    pub(crate) fn monitor(
        &self,
        watcher: Pid,
        target: Pid,
        alive: impl Fn(Pid) -> bool,
    ) -> (u64, bool) {
        let reference = self.next_ref.fetch_add(1, Ordering::Relaxed) + 1;
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        if !alive(target) {
            return (reference, false);
        }
        t.monitors
            .entry(target)
            .or_default()
            .insert(reference, watcher);
        t.watching
            .entry(watcher)
            .or_default()
            .insert(reference, target);
        (reference, true)
    }

    /// Remove `watcher`'s monitor `reference`. Unknown references are ignored.
    pub(crate) fn demonitor(&self, watcher: Pid, reference: u64) {
        let mut t = self.tables.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(target) = t
            .watching
            .get_mut(&watcher)
            .and_then(|m| m.remove(&reference))
        else {
            return;
        };
        if let Some(m) = t.monitors.get_mut(&target) {
            m.remove(&reference);
            if m.is_empty() {
                t.monitors.remove(&target);
            }
        }
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
        for (reference, target) in t.watching.remove(&pid).unwrap_or_default() {
            if let Some(m) = t.monitors.get_mut(&target) {
                m.remove(&reference);
                if m.is_empty() {
                    t.monitors.remove(&target);
                }
            }
        }
        ExitNotices { linked, monitors }
    }
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod links_tests;
