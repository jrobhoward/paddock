//! The process table: every live instance on this node, by id.

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};

use crate::instance::InstanceHandle;
use crate::pid::Pid;

/// Live instances by id. An entry is removed when its instance exits, so a
/// lookup of a dead pid, or of an earlier incarnation, finds nothing.
#[derive(Debug, Default)]
pub(crate) struct ProcessTable {
    entries: RwLock<HashMap<u64, InstanceHandle>>,
}

impl ProcessTable {
    /// The live instance with exactly this pid.
    pub(crate) fn lookup(&self, pid: Pid) -> Option<InstanceHandle> {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        entries.get(&pid.id).filter(|h| h.pid() == pid).cloned()
    }

    /// Add an instance if its id is free and the table has room. Returns the
    /// pid of whatever holds the id, or `None` if the table is full.
    pub(crate) fn insert(
        &self,
        handle: InstanceHandle,
        capacity: usize,
    ) -> Result<(), Option<Pid>> {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = entries.get(&handle.pid().id) {
            return Err(Some(existing.pid()));
        }
        if entries.len() >= capacity {
            return Err(None);
        }
        entries.insert(handle.pid().id, handle);
        Ok(())
    }

    /// Remove the instance with exactly this pid.
    pub(crate) fn remove(&self, pid: Pid) {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        if entries.get(&pid.id).is_some_and(|h| h.pid() == pid) {
            entries.remove(&pid.id);
        }
    }

    /// Number of live instances.
    pub(crate) fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}
