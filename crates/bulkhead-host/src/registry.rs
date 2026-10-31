//! The node-local name registry.

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};

use crate::pid::Pid;

/// Registered names on this node. An instance may hold several names; they
/// are all released when it exits, before its exit is published, so a
/// restarted instance can register them again.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    names: RwLock<HashMap<String, Pid>>,
}

impl Registry {
    /// Register `name` for `pid`. Registering a name the same pid already
    /// holds succeeds; a name held by another pid returns that pid.
    pub(crate) fn register(&self, name: &str, pid: Pid) -> Result<(), Pid> {
        let mut names = self.names.write().unwrap_or_else(PoisonError::into_inner);
        match names.get(name) {
            Some(holder) if *holder != pid => Err(*holder),
            _ => {
                names.insert(name.to_string(), pid);
                Ok(())
            }
        }
    }

    /// The pid holding `name`.
    pub(crate) fn whereis(&self, name: &str) -> Option<Pid> {
        self.names
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .copied()
    }

    /// Release every name `pid` holds.
    pub(crate) fn unregister_all(&self, pid: Pid) {
        self.names
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, holder| *holder != pid);
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;
