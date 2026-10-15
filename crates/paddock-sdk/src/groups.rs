//! Process groups across the cluster, with publish, through
//! `paddock:groups` (cargo feature `groups`), as OTP's `pg`: membership is
//! eventual, and a node cut off from another stops listing its members
//! until they hear each other again.
//!
//! Declare the `paddock:groups` capability in the manifest with the groups
//! the app may join and publish to. An instance leaves every group when it
//! exits; an upgrade keeps its groups. Published messages arrive as casts
//! (`handle_cast`).
//!
//! ```ignore
//! use paddock_sdk::{Envelope, groups};
//!
//! groups::join("prices")?;                                  // a subscriber
//! let reached = groups::publish("prices", Envelope::text("42.5"))?;  // a producer
//! ```

use std::fmt;

use crate::bindings::paddock::groups::groups as wit;
use crate::envelope::Envelope;
use crate::types::Pid;

/// Why a group operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GroupError {
    /// The operator did not grant `paddock:groups`, or the manifest does
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

impl From<wit::Error> for GroupError {
    fn from(e: wit::Error) -> Self {
        match e {
            wit::Error::AccessDenied => Self::AccessDenied,
            wit::Error::LimitReached => Self::LimitReached,
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
    Ok(wit::join(group)?)
}

/// Leave `group`; leaving a group this instance is not in does nothing.
///
/// # Errors
///
/// [`GroupError::AccessDenied`] as for [`join`].
pub fn leave(group: &str) -> Result<(), GroupError> {
    Ok(wit::leave(group)?)
}

/// The members of `group` on every node this node hears, ordered by pid.
///
/// # Errors
///
/// [`GroupError::AccessDenied`] unless the manifest lists `group` under
/// `join` or `publish` and the capability is granted.
pub fn members(group: &str) -> Result<Vec<Pid>, GroupError> {
    Ok(wit::members(group)?.into_iter().map(Pid::from).collect())
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
    Ok(wit::publish(group, &msg.into())?)
}
