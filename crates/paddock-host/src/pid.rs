//! Process identifiers.

use std::fmt;

/// Identifies one instance in the cluster: `{node, id, incarnation}`.
///
/// `id` stays the same when a supervisor restarts the instance;
/// `incarnation` goes up by one, so a pid held from before the restart never
/// reaches the new instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pid {
    /// Node hosting the instance.
    pub node: u32,
    /// Instance id, unique on its node and stable across restarts.
    pub id: u64,
    /// Restart count, starting at 1.
    pub incarnation: u32,
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}.{}.{}>", self.node, self.id, self.incarnation)
    }
}
