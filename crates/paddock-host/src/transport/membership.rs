//! Which nodes form the cluster (ROADMAP D2).
//!
//! A node joins by sending [`Frame::Join`](super::Frame) to one or more
//! seeds. A node that hears a join adds the newcomer, answers with every
//! member it knows ([`Frame::Members`](super::Frame)), and passes the
//! newcomer on to the other members. A node that learns of a member it did
//! not know sends it a join of its own, so every pair ends up connected (a
//! full mesh, as Erlang's `connect_all`). Each node heartbeats every member
//! it knows ([`super::liveness`]), and every ten heartbeats sends every
//! member it knows to each peer it reaches, so a node that missed a join's
//! answer still learns of every member.
//!
//! Membership and liveness are separate: a member that stops answering
//! stays a member, with its session ended, until it answers again or
//! leaves. [`Frame::Leave`](super::Frame) removes a member and ends its
//! session.
//!
//! Each start of a node has a creation number, raised on every start and
//! kept on disk by the node. A member seen with a higher creation has
//! restarted: its old session ends, and frames from the earlier creation,
//! or meant for it, are discarded.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// What a member does in the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum Role {
    /// Runs apps.
    #[default]
    Member,
    /// Runs no apps; takes part in failover decisions (ROADMAP D10).
    Witness,
}

/// One node of the cluster.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct Member {
    /// Its node id.
    pub node: u32,
    /// Its creation: raised on every start.
    pub creation: u32,
    /// Where the transport reaches it; its meaning is the transport's.
    pub address: String,
    /// What it does.
    pub role: Role,
}

impl Member {
    /// A member record.
    #[must_use]
    pub fn new(node: u32, creation: u32, address: impl Into<String>, role: Role) -> Self {
        Self {
            node,
            creation,
            address: address.into(),
            role,
        }
    }
}

/// The members this node knows, itself included.
#[derive(Debug)]
pub(crate) struct Membership {
    me: u32,
    members: Mutex<BTreeMap<u32, Member>>,
}

impl Membership {
    pub(crate) fn new(me: Member) -> Self {
        let node = me.node;
        Self {
            me: node,
            members: Mutex::new(BTreeMap::from([(node, me)])),
        }
    }

    fn members(&self) -> MutexGuard<'_, BTreeMap<u32, Member>> {
        self.members.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn me(&self) -> Member {
        self.members()[&self.me].clone()
    }

    /// Every member, by node id.
    pub(crate) fn list(&self) -> Vec<Member> {
        self.members().values().cloned().collect()
    }

    /// The other members' node ids.
    pub(crate) fn others(&self) -> Vec<u32> {
        self.members()
            .keys()
            .copied()
            .filter(|&n| n != self.me)
            .collect()
    }

    /// Add what `incoming` says. Returns the members that are new, or seen
    /// with a higher creation; an older creation of a known member is
    /// ignored.
    pub(crate) fn merge(&self, incoming: &[Member]) -> Vec<Member> {
        let mut members = self.members();
        let mut changed = Vec::new();
        for m in incoming.iter().filter(|m| m.node != self.me) {
            let newer = members
                .get(&m.node)
                .is_none_or(|old| m.creation > old.creation);
            if newer {
                members.insert(m.node, m.clone());
                changed.push(m.clone());
            }
        }
        changed
    }

    /// Forget `node`. Returns whether it was a member.
    pub(crate) fn remove(&self, node: u32) -> bool {
        node != self.me && self.members().remove(&node).is_some()
    }
}
