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

impl std::str::FromStr for Pid {
    type Err = String;

    /// Parse `node.id.incarnation`, with or without the angle brackets
    /// [`Pid`]'s `Display` adds.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bare = s.trim().trim_start_matches('<').trim_end_matches('>');
        let mut parts = bare.split('.');
        let err = || format!("'{s}' is not a pid (expected node.id.incarnation)");
        let (Some(node), Some(id), Some(incarnation), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(err());
        };
        Ok(Self {
            node: node.parse().map_err(|_| err())?,
            id: id.parse().map_err(|_| err())?,
            incarnation: incarnation.parse().map_err(|_| err())?,
        })
    }
}

#[cfg(test)]
#[path = "pid_tests.rs"]
mod pid_tests;
