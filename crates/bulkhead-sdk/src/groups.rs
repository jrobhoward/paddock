//! Process groups across the cluster, with publish, through
//! `bulkhead:groups` (cargo feature `groups`), as OTP's `pg`: membership is
//! eventual, and a node cut off from another stops listing its members
//! until they hear each other again.
//!
//! Declare the `bulkhead:groups` capability in the manifest with the groups
//! the app may join and publish to. An instance leaves every group when it
//! exits; an upgrade keeps its groups. Published messages arrive as casts
//! (`handle_cast`).
//!
//! ```ignore
//! use bulkhead_sdk::{Envelope, groups};
//!
//! groups::join("prices")?;                                  // a subscriber
//! let reached = groups::publish("prices", Envelope::text("42.5"))?;  // a producer
//! ```

use std::fmt;

use bulkhead_native::app::Ctx;
use bulkhead_native::protocol as p;

use crate::envelope::Envelope;
use crate::types::Pid;

/// Why a group operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GroupError {
    /// The operator did not grant `bulkhead:groups`, or the manifest does
    /// not list the group for this use.
    AccessDenied,
    /// Joining would exceed the instance's `limits.groups_max`.
    LimitReached,
}

impl fmt::Display for GroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("access denied"),
            Self::LimitReached => f.write_str("group limit reached"),
        }
    }
}

impl std::error::Error for GroupError {}

impl From<p::GroupsError> for GroupError {
    fn from(e: p::GroupsError) -> Self {
        match e {
            p::GroupsError::AccessDenied => Self::AccessDenied,
            p::GroupsError::LimitReached => Self::LimitReached,
        }
    }
}

/// Join `group`; joining again does nothing.
///
/// # Errors
///
/// [`GroupError::AccessDenied`] unless the manifest lists `group` under
/// `join` and the capability is granted; [`GroupError::LimitReached`] over
/// `limits.groups_max`.
pub fn join(group: &str) -> Result<(), GroupError> {
    Ok(Ctx::current().join_group(group)?)
}

/// Leave `group`; leaving a group this instance is not in does nothing.
///
/// # Errors
///
/// [`GroupError::AccessDenied`] as for [`join`].
pub fn leave(group: &str) -> Result<(), GroupError> {
    Ok(Ctx::current().leave_group(group)?)
}

/// The members of `group` on every node this node hears, ordered by pid.
///
/// # Errors
///
/// [`GroupError::AccessDenied`] unless the manifest lists `group` under
/// `join` or `publish` and the capability is granted.
pub fn members(group: &str) -> Result<Vec<Pid>, GroupError> {
    Ok(Ctx::current()
        .group_members(group)?
        .into_iter()
        .map(Pid::from)
        .collect())
}

/// Cast `msg` to every member of `group` on any node, this instance too if
/// it is one, except members the messaging ACLs keep apart. Returns how
/// many members took it: on this node, those whose mailboxes did; on
/// another node, all its members once the copy is sent. Members with a full
/// mailbox, or a `message_max_bytes` below the body's size, miss it.
///
/// # Errors
///
/// [`GroupError::AccessDenied`] unless the manifest lists `group` under
/// `publish` and the capability is granted.
pub fn publish(group: &str, msg: Envelope) -> Result<u32, GroupError> {
    Ok(Ctx::current().publish(group, msg.into())?)
}
