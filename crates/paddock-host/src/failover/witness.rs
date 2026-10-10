//! The witness: grants promotions and resumes, waiting out the other
//! node's lease (R3), one request at a time (R5).

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
        }
    }

    fn partner(&self, node: u32) -> Option<u32> {
        match self.cfg.pair {
            [a, b] if a == node => Some(b),
            [a, b] if b == node => Some(a),
            _ => None,
        }
    }

    pub(super) fn handle(&mut self, from: u32, msg: FailoverMsg) -> Option<FailoverMsg> {
        let partner = self.partner(from)?;
        let now = Instant::now();
        let hold = self.cfg.lease + self.cfg.margin;
        match msg {
            FailoverMsg::Renew { seq, epoch } => {
                if epoch == self.epoch && from == self.promoted {
                    self.renewed.insert(from, now);
                    Some(FailoverMsg::LeaseOk { seq })
                } else {
                    Some(FailoverMsg::Fenced { epoch: self.epoch })
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
                    return Some(FailoverMsg::Refused { epoch: self.epoch });
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
                    return Some(FailoverMsg::NotYet {
                        wait_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX) + 1,
                    });
                }
                if !resume {
                    self.epoch = epoch + 1;
                    self.promoted = from;
                }
                self.renewed.remove(&partner);
                self.renewed.insert(from, now);
                Some(FailoverMsg::Granted { epoch: self.epoch })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "witness_tests.rs"]
mod witness_tests;
