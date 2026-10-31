//! The witness: grants promotions and resumes, waiting out the other
//! node's lease (R3), one request at a time (R5), and keeps the record of
//! a partner out of date for replicated buckets (R13).
//!
//! A grant or a change to the record is decided, kept on disk, then
//! applied (R10): the state in memory never runs ahead of disk, so no
//! answer carries an epoch or a record a restart could lose. While a
//! change is being kept, the witness answers nothing from the pair: a
//! renewal of the node being replaced could otherwise extend its lease
//! after the grant was decided, and a second request would break R5.
//! Unanswered, both are as if lost.
//!
//! The record (spike S20's R1): the primary sets it, at its epoch, before
//! it commits a replicated write without its partner, and clears it once
//! the partner has caught up. While it stands, the witness refuses to
//! promote the partner, which may lack committed writes. A promotion
//! clears it: the new primary had every committed write, and starts
//! committing only with its partner's acknowledgement.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use super::{FailoverMsg, WitnessConfig, WitnessState};

#[derive(Debug)]
pub(super) struct Witness {
    pub(super) cfg: WitnessConfig,
    pub(super) epoch: u64,
    pub(super) promoted: u32,
    /// The partner of `promoted` is recorded out of date.
    pub(super) outdated: bool,
    /// The last renewal the witness granted each node.
    pub(super) renewed: HashMap<u32, Instant>,
    /// `promoted` stepped down at `epoch` in a planned switchover and gave
    /// up its lease (`Release`): its partner's request need not
    /// wait the lease out. Cleared by every grant; kept in memory only, so
    /// a restarted witness waits as before.
    pub(super) released: bool,
    /// A change decided and being kept on disk.
    keeping: bool,
}

/// What the witness does with a message.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    /// Answer at once.
    Answer(FailoverMsg),
    /// Grant: keep the state it leads to on disk, then [`Witness::apply`].
    Grant(Grant),
    /// Change the record: keep it on disk, then [`Witness::apply_record`].
    Record(Record),
}

/// A grant decided and not yet applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Grant {
    /// The node granted.
    pub(super) to: u32,
    /// The request granted, echoed in the answer.
    pub(super) seq: u64,
    /// The epoch it is granted at.
    pub(super) epoch: u64,
    /// The node promoted at that epoch.
    pub(super) promoted: u32,
    /// The record after the grant: cleared by a promotion, kept by a
    /// resume.
    pub(super) outdated: bool,
}

impl Grant {
    /// The witness's state once the grant is applied.
    pub(super) fn state(&self) -> WitnessState {
        WitnessState::new(self.epoch, self.promoted, self.outdated)
    }
}

/// A change to the record, decided and not yet applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Record {
    /// The primary that asked, at the witness's epoch.
    pub(super) to: u32,
    /// Its request, echoed in the answer.
    pub(super) seq: u64,
    /// The record asked for.
    pub(super) outdated: bool,
    /// The witness's state once it is applied.
    pub(super) state: WitnessState,
}

impl Witness {
    /// A witness starting at epoch 1 with `cfg.first` and no record, or
    /// resuming the state kept on disk. A resumed witness counts the node it promoted as renewed
    /// now: its renewals before the restart were lost, and none can be later
    /// than this, so grants wait at least as long as with the true record
    /// (R10, which the model keeps across crashes).
    pub(super) fn start(cfg: WitnessConfig) -> Self {
        let state = cfg
            .resume
            .unwrap_or_else(|| WitnessState::new(1, cfg.first, false));
        let mut renewed = HashMap::new();
        if cfg.resume.is_some() {
            renewed.insert(state.promoted, Instant::now());
        }
        Self {
            cfg,
            epoch: state.epoch,
            promoted: state.promoted,
            outdated: state.outdated,
            renewed,
            released: false,
            keeping: false,
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
        if self.keeping {
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
                seq,
                resume,
                epoch,
                confirmed_ago_ms,
            } => {
                // A node recorded out of date is not promoted (R13).
                let allowed = if resume {
                    from == self.promoted
                } else {
                    epoch + 1 > self.epoch && !(self.outdated && from != self.promoted)
                };
                if !allowed {
                    return answer(FailoverMsg::Refused { epoch: self.epoch });
                }
                // The partner's lease is gone by the witness's record of
                // renewals and by the requester's last confirmation (R3),
                // or at once if the partner released it (R14).
                let confirmed = now
                    .checked_sub(Duration::from_millis(confirmed_ago_ms))
                    .unwrap_or(now);
                let mut gone_at = confirmed + hold;
                if let Some(renewed) = self.renewed.get(&partner) {
                    gone_at = gone_at.max(*renewed + hold);
                }
                if self.released && !resume && partner == self.promoted {
                    gone_at = now;
                }
                if now < gone_at {
                    let wait = gone_at.duration_since(now);
                    return answer(FailoverMsg::NotYet {
                        wait_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX) + 1,
                    });
                }
                self.keeping = true;
                Some(Decision::Grant(if resume {
                    Grant {
                        to: from,
                        seq,
                        epoch: self.epoch,
                        promoted: self.promoted,
                        outdated: self.outdated,
                    }
                } else {
                    Grant {
                        to: from,
                        seq,
                        epoch: epoch + 1,
                        promoted: from,
                        outdated: false,
                    }
                }))
            }
            FailoverMsg::Release { epoch } => {
                // Only the node promoted at the witness's epoch (R14). In
                // order behind its renewals and before any later request of
                // its own, on the one connection between them.
                if epoch == self.epoch && from == self.promoted {
                    self.released = true;
                }
                None
            }
            FailoverMsg::Record {
                seq,
                epoch,
                outdated,
            } => {
                // Only the node promoted at the witness's epoch (R13).
                if epoch != self.epoch || from != self.promoted {
                    return answer(FailoverMsg::Fenced { epoch: self.epoch });
                }
                if outdated == self.outdated {
                    return answer(FailoverMsg::Recorded { seq, outdated });
                }
                self.keeping = true;
                Some(Decision::Record(Record {
                    to: from,
                    seq,
                    outdated,
                    state: WitnessState::new(self.epoch, self.promoted, outdated),
                }))
            }
            _ => None,
        }
    }

    /// Apply `grant`, now kept on disk, and return the answer to send.
    pub(super) fn apply(&mut self, grant: Grant) -> FailoverMsg {
        self.keeping = false;
        self.released = false;
        self.epoch = grant.epoch;
        self.promoted = grant.promoted;
        self.outdated = grant.outdated;
        if let Some(partner) = self.partner(grant.to) {
            self.renewed.remove(&partner);
        }
        self.renewed.insert(grant.to, Instant::now());
        FailoverMsg::Granted {
            seq: grant.seq,
            epoch: grant.epoch,
            outdated: grant.outdated,
        }
    }

    /// Apply `record`, now kept on disk, and return the answer to send.
    pub(super) fn apply_record(&mut self, record: Record) -> FailoverMsg {
        self.keeping = false;
        self.outdated = record.outdated;
        FailoverMsg::Recorded {
            seq: record.seq,
            outdated: record.outdated,
        }
    }

    /// Drop a change that could not be kept; the requester asks again.
    pub(super) fn abandon(&mut self) {
        self.keeping = false;
    }
}

#[cfg(test)]
#[path = "witness_tests.rs"]
mod witness_tests;
