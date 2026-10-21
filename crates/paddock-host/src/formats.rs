//! The pair's version for what is kept (ROADMAP U1; DESIGN.md, "Mixed
//! versions during a release"): the protocol version whose formats the
//! nodes of an HA pair write to what outlives a connection, the
//! replicated log and the `@cluster` records among them.
//!
//! It is kept in a reserved replicated bucket (`@release`, key
//! `formats`), so both nodes apply a change at the same index of the log,
//! and it rises only by [`Runtime::finalize_formats`], once every node
//! named speaks this build's version. Until then a node writes the formats
//! of the version recorded, so either node can be rolled back to the build
//! before. A node reads it from its own copy, which needs no replication
//! running, and refuses to start a build older than it
//! ([`Runtime::check_formats`]).
//!
//! No kept format has changed since the version was introduced, so
//! nothing reads it yet to choose a format: a release that changes one
//! ships the new reader at once and writes the new format only once
//! [`Runtime::formats_version`] has reached the version that brought it.

use paddock_bundle::manifest::Durability;

use crate::error::HostError;
use crate::replication::SPACE;
use crate::runtime::Runtime;
use crate::services::keyvalue::storage::{BucketHandle, Op, Outcome};
use crate::transport::wire;

/// The replicated bucket holding the version. Shared and app bucket names
/// are valid names, which never start with `@`.
const BUCKET: &str = "@release";

/// Its key.
const KEY: &str = "formats";

/// The version of what was kept before the version was recorded: the
/// formats of wire version 6, which version 7 did not change.
pub const FIRST_FORMATS: u16 = 6;

/// What [`Runtime::finalize_formats`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Finalized {
    /// The pair's version was raised to this one.
    Raised(u16),
    /// The pair's version was this one already.
    Already(u16),
}

/// Why the pair's version was not raised.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FinalizeRefused {
    /// Without replication nothing kept is shared with the partner.
    #[error("replication does not run on this node")]
    NoReplication,
    /// A node that must speak the new version is not connected, or agreed
    /// on an older one.
    #[error("node {node} {}", match .version {
        Some(v) => format!("speaks version {v}, not {}", wire::VERSION),
        None => "is not connected".into(),
    })]
    Peer {
        /// The node.
        node: u32,
        /// The version its connection agreed on, if it is connected.
        version: Option<u16>,
    },
    /// The record could not be read or did not commit (no primary holding
    /// its lease, or the standby and the witness out of reach).
    #[error("the pair's version: {0}")]
    Store(String),
}

fn bucket() -> BucketHandle {
    BucketHandle {
        space: SPACE.to_string(),
        name: BUCKET.to_string(),
        durability: Durability::Replicated,
        quota: None,
        writable: true,
    }
}

impl Runtime {
    /// The pair's version for what is kept, from this node's own copy:
    /// [`FIRST_FORMATS`] until [`Runtime::finalize_formats`] first raised
    /// it, and on a node without a data directory.
    ///
    /// # Errors
    ///
    /// [`FinalizeRefused::Store`] if this node's copy cannot be read, or
    /// holds something that is not a version.
    pub async fn formats_version(&self) -> Result<u16, FinalizeRefused> {
        let kv = self.inner().services.keyvalue();
        if kv.disk().is_none() {
            return Ok(FIRST_FORMATS);
        }
        match kv.run(&bucket(), Op::Get(KEY.to_string())).await {
            Ok(Outcome::Value(Some(bytes))) => wire::from_cbor(&bytes)
                .map_err(|e| FinalizeRefused::Store(format!("malformed record: {e}"))),
            Ok(_) => Ok(FIRST_FORMATS),
            Err(e) => Err(FinalizeRefused::Store(e.to_string())),
        }
    }

    /// Refuse to run if the pair's version is newer than this build
    /// speaks: the node was finalized by a later build, whose formats this
    /// one may not read. Call before anything runs.
    ///
    /// # Errors
    ///
    /// [`HostError::Engine`] naming both versions, or if the version
    /// cannot be read.
    pub async fn check_formats(&self) -> Result<(), HostError> {
        let kept = self
            .formats_version()
            .await
            .map_err(|e| HostError::Engine(e.to_string()))?;
        if kept > wire::VERSION {
            return Err(HostError::Engine(format!(
                "the pair was finalized at version {kept}, and this build speaks up to version {}: \
                 run a build of that release or later",
                wire::VERSION
            )));
        }
        Ok(())
    }

    /// Raise the pair's version to this build's [`wire::VERSION`], the
    /// last step of a release: from then on neither node can be rolled
    /// back past it. Every node in `peers` (the partner and the witness)
    /// must be connected at that version. Whether it was there already is
    /// read through the primary, since this node's copy may lag.
    ///
    /// # Errors
    ///
    /// [`FinalizeRefused`]: no replication here, a peer not connected at
    /// this build's version, or a record that cannot be read or does not
    /// commit.
    pub async fn finalize_formats(&self, peers: &[u32]) -> Result<Finalized, FinalizeRefused> {
        let kv = self.inner().services.keyvalue();
        if kv.replication().is_none() {
            return Err(FinalizeRefused::NoReplication);
        }
        for &node in peers {
            let version = self.peer_version(node);
            if version.is_none_or(|v| v < wire::VERSION) {
                return Err(FinalizeRefused::Peer { node, version });
            }
        }
        // Read through the primary: this node's copy may not have applied
        // a raise from the other node yet.
        let kept = match kv
            .run_linearizable(&bucket(), Op::Get(KEY.to_string()))
            .await
        {
            Ok(Outcome::Value(Some(bytes))) => wire::from_cbor(&bytes)
                .map_err(|e| FinalizeRefused::Store(format!("malformed record: {e}")))?,
            Ok(_) => FIRST_FORMATS,
            Err(e) => return Err(FinalizeRefused::Store(e.to_string())),
        };
        if kept >= wire::VERSION {
            return Ok(Finalized::Already(kept));
        }
        let bytes =
            wire::to_cbor(&wire::VERSION).map_err(|e| FinalizeRefused::Store(e.to_string()))?;
        kv.run(&bucket(), Op::Set(KEY.to_string(), bytes))
            .await
            .map_err(|e| FinalizeRefused::Store(e.to_string()))?;
        Ok(Finalized::Raised(wire::VERSION))
    }

    /// The protocol version the connection to `node` agreed on, if this
    /// node is connected to it ([`crate::transport::Transport::version`]).
    #[must_use]
    pub fn peer_version(&self, node: u32) -> Option<u16> {
        self.inner().transport()?.version(node)
    }
}

#[cfg(test)]
#[path = "formats_tests.rs"]
mod formats_tests;
