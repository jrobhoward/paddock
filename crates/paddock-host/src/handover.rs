//! Moving a singleton to the other node of an HA pair with its state
//! (ROADMAP M1): in a planned switchover the old primary takes the
//! instance's snapshot and writes it to the pair's replicated log; the
//! node that next starts the singleton loads it in place of `init`.
//!
//! The record is kept in a reserved replicated bucket (`@handover`, one
//! key per app) and every start of a singleton that moves takes it first:
//! [`Runtime::take_handover`] deletes the record and waits for the delete
//! to commit before the instance runs. So a record that is present was
//! written after the last start, and no node has run the singleton since:
//! loading it is as current as the state the old primary stopped with,
//! whatever happened to the leadership in between. Its reads come from
//! the node's own copy, which replication has current before a singleton
//! with replicated state starts (R5).
//!
//! The moved instance gets a new pid on its new node. Links, monitors and
//! registered names are not carried; the old instance exits `normal`, so
//! links do not carry its exit, and messages left in its mailbox are
//! dropped.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use paddock_bundle::manifest::Durability;

use crate::error::HostError;
use crate::replication::SPACE;
use crate::runtime::{Runtime, RuntimeInner};
use crate::services::keyvalue::storage::{BucketHandle, Op, Outcome};
use crate::transport::wire;

/// The replicated bucket holding handover records, by app. Shared and app
/// bucket names are valid names, which never start with `@`.
const BUCKET: &str = "@handover";

/// A singleton's state, on its way to the node that starts it next.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct Handover {
    /// The version that wrote the snapshot, passed to `load-snapshot`.
    pub version: String,
    /// `save-snapshot`'s output.
    #[serde(with = "crate::transport::wire::bytes")]
    pub snapshot: Vec<u8>,
}

impl Handover {
    /// A snapshot written by `version`.
    #[must_use]
    pub fn new(version: impl Into<String>, snapshot: Vec<u8>) -> Self {
        Self {
            version: version.into(),
            snapshot,
        }
    }
}

/// Handovers waiting for their app's first instance to start, by app.
#[derive(Debug, Default)]
pub(crate) struct Pending(Mutex<HashMap<String, Handover>>);

impl Pending {
    pub(crate) fn put(&self, app: &str, handover: Handover) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(app.to_string(), handover);
    }

    /// The handover for `app`'s next start, once.
    pub(crate) fn take(&self, app: &str) -> Option<Handover> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(app)
    }
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

fn unavailable(why: impl std::fmt::Display) -> HostError {
    HostError::Handover(why.to_string())
}

impl RuntimeInner {
    async fn handover_op(&self, op: Op) -> Result<Outcome, HostError> {
        let kv = self.services.keyvalue();
        if kv.replication().is_none() {
            return Err(unavailable("replication is not running"));
        }
        kv.run(&bucket(), op).await.map_err(unavailable)
    }
}

impl Runtime {
    /// Whether singletons move with their state on this node: replication
    /// runs (`[failover] replication = true`, or a `cluster` bucket).
    #[must_use]
    pub fn moves_state(&self) -> bool {
        self.inner().services.keyvalue().replication().is_some()
    }

    /// Keep `handover` for the next start of `app`, on either node of the
    /// pair; returns once it has committed.
    ///
    /// # Errors
    ///
    /// [`HostError::Handover`] if replication does not run here, or the
    /// write does not commit (no primary holding its lease, or the standby
    /// and the witness out of reach).
    pub async fn put_handover(&self, app: &str, handover: &Handover) -> Result<(), HostError> {
        let bytes = wire::to_cbor(handover).map_err(unavailable)?;
        self.inner()
            .handover_op(Op::Set(app.to_string(), bytes))
            .await
            .map(|_| ())
    }

    /// The handover record for `app`, if any, deleted: call before every
    /// start of a singleton that moves, and start it only on `Ok`. The
    /// delete has committed when this returns, so a later start on either
    /// node finds no record unless a new one is written.
    ///
    /// # Errors
    ///
    /// [`HostError::Handover`] if replication does not run here, or the
    /// delete does not commit; the record may still be there.
    pub async fn take_handover(&self, app: &str) -> Result<Option<Handover>, HostError> {
        let rt = self.inner();
        let found = match rt.handover_op(Op::Get(app.to_string())).await? {
            Outcome::Value(Some(bytes)) => Some(bytes),
            _ => None,
        };
        let Some(bytes) = found else {
            return Ok(None);
        };
        rt.handover_op(Op::Delete(app.to_string())).await?;
        match wire::from_cbor::<Handover>(&bytes) {
            Ok(handover) => Ok(Some(handover)),
            Err(e) => {
                tracing::warn!(app, error = %e, "malformed handover record; dropped");
                Ok(None)
            }
        }
    }
}
