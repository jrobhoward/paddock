//! The witness: grants promotions and resumes, waiting out the other
//! node's lease (R3), one request at a time (R5).
//!
//! A grant is decided, kept on disk, then applied (R10): the epoch and
//! promoted node in memory never run ahead of disk, so no answer carries
//! an epoch a restart could lose. While a grant is being kept, the witness
//! answers nothing from the pair: a renewal of the node being replaced
//! could otherwise extend its lease after the grant was decided, and a
//! second request would break R5. Unanswered, both are as if lost.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use super::{FailoverMsg, WitnessConfig};

#[derive(Debug)]
pub(super) struct Witness {
    pub(super) cfg: WitnessConfig,
    pub(super) epoch: u64,
    pub(super) promoted: u32,
    /// The last renewal the witness granted each node.
    pub(super) renewed: HashMap<u32, Instant>,
    /// A grant decided and being kept on disk.
    granting: bool,
}

/// What the witness does with a message.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    /// Answer at once.
    Answer(FailoverMsg),
    /// Grant: keep `epoch` and `promoted` on disk, then [`Witness::apply`].
    Grant(Grant),
}

/// A grant decided and not yet applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Grant {
    /// The node granted.
    pub(super) to: u32,
    /// The epoch it is granted at.
    pub(super) epoch: u64,
    /// The node promoted at that epoch.
    pub(super) promoted: u32,
}

impl Witness {
    /// A witness starting at epoch 1 with `cfg.first`, or resuming the state
    /// kept on disk. A resumed witness counts the node it promoted as renewed
    /// now: its renewals before the restart were lost, and none can be later
    /// than this, so grants wait at least as long as with the true record
    /// (R10, which the model keeps across crashes).
    pub(super) fn start(cfg: WitnessConfig) -> Self {
        let (epoch, promoted) = cfg.resume.unwrap_or((1, cfg.first));
        let mut renewed = HashMap::new();
        if cfg.resume.is_some() {
            renewed.insert(promoted, Instant::now());
        }
        Self {
            cfg,
            epoch,
            promoted,
            renewed,
            granting: false,
        }
    }

    fn partner(&self, node: u32) -> Option<u32> {
        match self.cfg.pair {
            [a, b] if a == node => Some(b),
            [a, b] if b == node => Some(a),
            _ => None,
        }
    }

    pub(super) fn handle(&mut self, from: u32, msg: FailoverMsg) -> Option<Decision> {
        let partner = self.partner(from)?;
        if self.granting {
            return None;
        }
        let now = Instant::now();
        let hold = self.cfg.lease + self.cfg.margin;
        let answer = |msg| Some(Decision::Answer(msg));
        match msg {
            FailoverMsg::Renew { seq, epoch } => {
                if epoch == self.epoch && from == self.promoted {
                    self.renewed.insert(from, now);
                    answer(FailoverMsg::LeaseOk { seq })
                } else {
                    answer(FailoverMsg::Fenced { epoch: self.epoch })
                }
            }
            FailoverMsg::Request {
                resume,
                epoch,
                confirmed_ago_ms,
            } => {
                let allowed = if resume {
                    from == self.promoted
                } else {
                    epoch + 1 > self.epoch
                };
                if !allowed {
                    return answer(FailoverMsg::Refused { epoch: self.epoch });
                }
                // The partner's lease is gone by the witness's record of
                // renewals and by the requester's last confirmation (R3).
                let confirmed = now
                    .checked_sub(Duration::from_millis(confirmed_ago_ms))
                    .unwrap_or(now);
                let mut gone_at = confirmed + hold;
                if let Some(renewed) = self.renewed.get(&partner) {
                    gone_at = gone_at.max(*renewed + hold);
                }
                if now < gone_at {
                    let wait = gone_at.duration_since(now);
                    return answer(FailoverMsg::NotYet {
                        wait_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX) + 1,
                    });
                }
                self.granting = true;
                Some(Decision::Grant(if resume {
                    Grant {
                        to: from,
                        epoch: self.epoch,
                        promoted: self.promoted,
                    }
                } else {
                    Grant {
                        to: from,
                        epoch: epoch + 1,
                        promoted: from,
                    }
                }))
            }
            _ => None,
        }
    }

    /// Apply `grant`, now kept on disk, and return the answer to send.
    pub(super) fn apply(&mut self, grant: Grant) -> FailoverMsg {
        self.granting = false;
        self.epoch = grant.epoch;
        self.promoted = grant.promoted;
        if let Some(partner) = self.partner(grant.to) {
            self.renewed.remove(&partner);
        }
        self.renewed.insert(grant.to, Instant::now());
        FailoverMsg::Granted { epoch: grant.epoch }
    }

    /// Drop a grant that could not be kept; the requester asks again.
    pub(super) fn abandon(&mut self) {
        self.granting = false;
    }
}

#[cfg(test)]
#[path = "witness_tests.rs"]
mod witness_tests;
