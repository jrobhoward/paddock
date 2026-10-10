//! Process identifiers.

use std::fmt;

/// Identifies one instance in the cluster: `{node, id, incarnation}`.
///
/// `id` stays the same when a supervisor restarts the instance;
/// `incarnation` goes up by one, so a pid held from before the restart never
/// reaches the new instance.
///
/// `id` holds the node's creation in its high 24 bits and a serial number,
/// counting from 1 on each start of the node, in its low 40, so a pid held
/// from before a node restarted never reaches an instance started after
/// ([`crate::transport::membership`]). Pids print as
/// `<node.serial.incarnation@creation>`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Pid {
    /// Node hosting the instance.
    pub node: u32,
    /// Instance id, unique on its node and stable across restarts.
    pub id: u64,
    /// Restart count, starting at 1.
    pub incarnation: u32,
}

/// Bits of an instance id that hold the serial number; the rest hold the
/// creation.
pub const SERIAL_BITS: u32 = 40;
/// The largest creation an instance id can hold.
pub const MAX_CREATION: u32 = (1 << (64 - SERIAL_BITS)) - 1;

/// The instance id for `serial` on a node's `creation` (at most
/// [`MAX_CREATION`]; `serial` below 2^40).
#[must_use]
pub const fn instance_id(creation: u32, serial: u64) -> u64 {
    ((creation as u64) << SERIAL_BITS) | (serial & ((1 << SERIAL_BITS) - 1))
}

impl Pid {
    /// The creation of the node that started the instance.
    #[must_use]
    pub const fn creation(&self) -> u32 {
        (self.id >> SERIAL_BITS) as u32
    }

    /// The instance's serial number within its node's start.
    #[must_use]
    pub const fn serial(&self) -> u64 {
        self.id & ((1 << SERIAL_BITS) - 1)
    }

    /// `node.serial.incarnation@creation`: [`Display`](fmt::Display)
    /// without the angle brackets.
    #[must_use]
    pub fn text(&self) -> String {
        format!(
            "{}.{}.{}@{}",
            self.node,
            self.serial(),
            self.incarnation,
            self.creation()
        )
    }
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.text())
    }
}

impl std::str::FromStr for Pid {
    type Err = String;

    /// Parse `node.serial.incarnation@creation` (as [`Pid`] displays), or
    /// `node.id.incarnation` with the whole id, with or without angle
    /// brackets.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bare = s.trim().trim_start_matches('<').trim_end_matches('>');
        let err = || {
            format!(
                "'{s}' is not a pid (expected node.serial.incarnation@creation \
                 or node.id.incarnation)"
            )
        };
        let (fields, creation) = match bare.split_once('@') {
            Some((fields, creation)) => (fields, Some(creation)),
            None => (bare, None),
        };
        let mut parts = fields.split('.');
        let (Some(node), Some(id), Some(incarnation), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(err());
        };
        let id = match creation {
            Some(creation) => instance_id(
                creation.parse().map_err(|_| err())?,
                id.parse().map_err(|_| err())?,
            ),
            None => id.parse().map_err(|_| err())?,
        };
        Ok(Self {
            node: node.parse().map_err(|_| err())?,
            id,
            incarnation: incarnation.parse().map_err(|_| err())?,
        })
    }
}

#[cfg(test)]
#[path = "pid_tests.rs"]
mod pid_tests;
