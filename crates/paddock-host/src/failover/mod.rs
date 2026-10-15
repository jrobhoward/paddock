//! Failover of singleton apps between an HA pair (ROADMAP D10), following
//! the rules spike S16 model-checked (`spikes/s16/README.md`, R1 to R7).
//!
//! Each node of the pair is `Primary`, `Standby` or `Down`, at an epoch.
//! Singletons may run only where [`Leadership::may_run`] is set: on the
//! primary, while it holds a lease. Who decides a promotion is the
//! [`Arbiter`]:
//!
//! - [`Arbiter::Witness`]: a third node ([`Runtime::serve_witness`]) grants
//!   each promotion. The primary's lease is renewed through its partner (the
//!   standby's beats say "standby, not promoting, epoch e" on every path,
//!   the serial line included: R1) or at the witness, only at the witness's
//!   epoch (R2). The standby asks to be promoted when it hears its partner on
//!   no path, and stops confirming the partner's lease from then on (R4).
//!   The witness grants only once the partner's lease has run out, by its
//!   own record of renewals and by the requester's report of when it last
//!   confirmed one (R3), one request at a time (R5). A primary stops its
//!   singletons `margin` before its lease would end, by its own clock (R6).
//!   Without the witness nothing is promoted: only while the witness
//!   cannot be reached either is an operator asked
//!   ([`Leadership::awaiting_confirmation`]), and a confirmation then
//!   promotes after waiting out the lease (R7).
//!   A grant's lease runs from when the request it answers was sent.
//! - [`Arbiter::Paths`]: the standby promotes itself when every heartbeat
//!   path is silent. No lease: safe when the serial line fails only with a
//!   node (S16's `NoLeaseSafe`).
//! - [`Arbiter::Operator`]: as `Paths`, but only once an operator confirms.
//!
//! A primary that meets a higher epoch, from its partner or at the witness,
//! steps down. A node that starts, or steps down, is `Down`: it rejoins a
//! primary it hears, or, with a witness, resumes as primary if the witness
//! last promoted it.
//!
//! Timing assumption (R11 in `docs/tla/VERIFICATION.md`): over one lease,
//! the clocks of the pair and the witness drift apart by less than twice
//! `margin` less the time a node takes to stop its singletons once its
//! lease ends (an epoch tick of guest code and a host call in flight; see
//! `RunUntil`). Message delay is not part of it, but for one bound: a lease
//! runs from when its renewal, or the stamp a confirmation echoes, was
//! sent, and the witness counts from when it received, so a delay only
//! makes the witness wait longer, and a grant's lease runs from the send
//! time of the request it answers (numbered, like renewals).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::Instant;

mod node;
mod witness;

use node::Node;
use witness::{Decision, Grant, Witness};

use crate::error::HostError;
use crate::runtime::{Runtime, RuntimeInner};
use crate::transport::{self, Frame};

/// Who decides that the standby becomes primary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Arbiter {
    /// The witness on this node id grants promotions; leases are used.
    Witness(u32),
    /// The standby promotes itself when its partner is silent on every path.
    Paths,
    /// The standby promotes itself when an operator confirms.
    Operator,
}

/// How a node of the pair fails over.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FailoverConfig {
    /// The other node of the pair.
    pub partner: u32,
    /// Who decides promotions.
    pub arbiter: Arbiter,
    /// How long a renewal keeps a lease.
    pub lease: Duration,
    /// Safety margin: a holder stops this long before its lease ends, and
    /// the witness waits this long after. Twice the margin must exceed the
    /// clocks' drift over a lease plus the time to stop a singleton; message
    /// delay does not count ([`crate::failover`]).
    pub margin: Duration,
    /// The epoch to start at: the highest this node reached before it
    /// stopped, kept on disk, so epochs (fencing tokens) never go back. 0
    /// for a node's first start.
    pub epoch: u64,
}

impl FailoverConfig {
    /// A configuration with a 3 s lease and a 500 ms margin, starting at
    /// epoch 0.
    #[must_use]
    pub fn new(partner: u32, arbiter: Arbiter) -> Self {
        Self {
            partner,
            arbiter,
            lease: Duration::from_secs(3),
            margin: Duration::from_millis(500),
            epoch: 0,
        }
    }
}

/// Keeps the witness's state durable: called with the new epoch and the
/// node promoted at it before a grant is answered. An error withholds the
/// grant (the requester asks again).
pub type WitnessPersist = Arc<dyn Fn(u64, u32) -> Result<(), String> + Send + Sync>;

/// How the witness arbitrates one pair.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WitnessConfig {
    /// The pair.
    pub pair: [u32; 2],
    /// The node the witness names primary at epoch 1.
    pub first: u32,
    /// As in [`FailoverConfig`]; both must agree.
    pub lease: Duration,
    /// As in [`FailoverConfig`]; both must agree.
    pub margin: Duration,
    /// The epoch and the node promoted at it, as last kept on disk, for a
    /// witness that restarts; `None` starts at epoch 1 with `first`.
    pub resume: Option<(u64, u32)>,
}

impl WitnessConfig {
    /// A configuration with a 3 s lease and a 500 ms margin.
    #[must_use]
    pub fn new(pair: [u32; 2], first: u32) -> Self {
        Self {
            pair,
            first,
            lease: Duration::from_secs(3),
            margin: Duration::from_millis(500),
            resume: None,
        }
    }
}

/// A node's standing in its pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Standing {
    /// Runs the singletons while it holds a lease.
    Primary,
    /// Takes over when the primary is gone.
    Standby,
    /// Starting, or stepped down: neither.
    Down,
}

/// What a node of the pair may do now.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Leadership {
    /// Its standing.
    pub standing: Standing,
    /// Its epoch: raised by every promotion.
    pub epoch: u64,
    /// Whether singletons may run here now.
    pub may_run: bool,
    /// The partner looks gone, no witness can decide (none is configured,
    /// or it cannot be reached), and only an operator's
    /// [`FailoverHandle::confirm`] would promote this node.
    pub awaiting_confirmation: bool,
}

/// Messages between a node of the pair and the witness.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailoverMsg {
    /// Renew the sender's lease at `epoch`; `seq` comes back in the answer.
    Renew {
        /// Echoed in the answer.
        seq: u64,
        /// The sender's epoch.
        epoch: u64,
    },
    /// Promote the sender (or, with `resume`, let the node the witness last
    /// promoted take up the role again).
    Request {
        /// Echoed in [`FailoverMsg::Granted`].
        seq: u64,
        /// A restarted or stepped-down node resuming, not a standby.
        resume: bool,
        /// The sender's epoch.
        epoch: u64,
        /// How long ago the sender last confirmed its partner's lease.
        confirmed_ago_ms: u64,
    },
    /// The lease is renewed.
    LeaseOk {
        /// From [`FailoverMsg::Renew`].
        seq: u64,
    },
    /// The sender's epoch is stale; the witness's is `epoch`.
    Fenced {
        /// The witness's epoch.
        epoch: u64,
    },
    /// The request is granted: the sender is primary at `epoch`, with a
    /// lease from when it sent the request numbered `seq`.
    Granted {
        /// From [`FailoverMsg::Request`].
        seq: u64,
        /// The new epoch.
        epoch: u64,
    },
    /// Ask again in `wait_ms`: the partner's lease has not run out.
    NotYet {
        /// How long until it has.
        wait_ms: u64,
    },
    /// The request cannot be granted; the witness's epoch is `epoch`.
    Refused {
        /// The witness's epoch.
        epoch: u64,
    },
}

/// A node's failover, once started: its leadership, and an operator's
/// confirmation.
#[derive(Debug, Clone)]
pub struct FailoverHandle {
    leadership: watch::Receiver<Leadership>,
    confirm: Arc<Notify>,
    switchover: mpsc::UnboundedSender<SwitchoverAsk>,
}

/// A switchover asked of the failover task: to check only, or to do.
pub(super) struct SwitchoverAsk {
    pub(super) check_only: bool,
    pub(super) reply: oneshot::Sender<Result<(), SwitchoverRefused>>,
}

/// Why a node did not hand over in a planned switchover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SwitchoverRefused {
    /// Only a primary hands over.
    #[error("this node is not the primary")]
    NotPrimary,
    /// A switchover hands over through the witness's grant; `paths` and
    /// `operator` pairs have no lease to hand over.
    #[error("a planned switchover needs arbiter \"witness\"")]
    NoWitness,
    /// It runs by an operator's override, without a lease.
    #[error("this primary runs by an operator's override; it has no lease to hand over")]
    Overridden,
    /// No live path says the partner is a standby confirming this node's
    /// lease, so nobody would take over.
    #[error("the partner is not heard as a confirming standby")]
    NoStandby,
    /// The failover task has ended (the runtime is gone).
    #[error("failover is not running")]
    Stopped,
}

impl FailoverHandle {
    /// The current leadership.
    #[must_use]
    pub fn leadership(&self) -> Leadership {
        self.leadership.borrow().clone()
    }

    /// Changes in leadership, as they happen.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Leadership> {
        self.leadership.clone()
    }

    /// An operator confirms that the partner is gone. Taken only while
    /// [`Leadership::awaiting_confirmation`] is set (with a witness, while
    /// the partner is silent on every path and the witness cannot be
    /// reached either), and ignored otherwise. Once taken it is final: the
    /// standby is promoted once the partner's lease has run out, even if the
    /// witness is reached again meanwhile, and with a witness runs without a
    /// lease until its partner confirms it as standby. It lapses only if the
    /// standby hears its partner again, steps down or rejoins first, so it
    /// never promotes on a later silence.
    pub fn confirm(&self) {
        self.confirm.notify_one();
    }

    /// A planned switchover (ROADMAP D10b): this node, a primary with a
    /// lease whose partner is heard as a confirming standby, steps down at
    /// once and yields; its singletons may no longer run. The standby, on
    /// hearing the yield, asks the witness at once instead of after hearing
    /// nothing for the node timeout, and the witness grants by its usual
    /// rule: once the lease plus margin has passed since the standby's last
    /// confirmation. This node rejoins as the new primary's standby; if
    /// nobody takes over, it may ask to resume a lease after a down node
    /// would. Stop the singletons gracefully first, while the lease holds:
    /// the step down only stops them running guest code.
    ///
    /// # Errors
    ///
    /// [`SwitchoverRefused`] when this node is not a primary with a lease
    /// and a confirming standby, or failover has stopped.
    pub async fn switchover(&self) -> Result<(), SwitchoverRefused> {
        self.ask_switchover(false).await
    }

    /// Whether [`FailoverHandle::switchover`] would hand over now, without
    /// doing it: to check before stopping the singletons.
    ///
    /// # Errors
    ///
    /// As for [`FailoverHandle::switchover`].
    pub async fn check_switchover(&self) -> Result<(), SwitchoverRefused> {
        self.ask_switchover(true).await
    }

    async fn ask_switchover(&self, check_only: bool) -> Result<(), SwitchoverRefused> {
        let (reply, answer) = oneshot::channel();
        self.switchover
            .send(SwitchoverAsk { check_only, reply })
            .map_err(|_| SwitchoverRefused::Stopped)?;
        answer.await.unwrap_or(Err(SwitchoverRefused::Stopped))
    }
}

/// A pair node's inbox for its witness's answers.
struct Inbox {
    /// The only node whose answers are taken: the configured witness, or
    /// none with another arbiter.
    witness: Option<u32>,
    tx: mpsc::UnboundedSender<FailoverMsg>,
}

/// Until when this node's singleton instances may run guest code: the
/// failover task's view of its lease, which each such instance checks
/// against the monotonic clock before every callback and at every epoch
/// tick ([`crate::InstanceSpec::bound_to_lease`]). The gate stops
/// singletons when the lease ends, but only once the failover task runs;
/// after the process was paused past its lease, instances may run first,
/// and this check keeps them from running guest code meanwhile
/// (assumption A8 in `docs/tla/VERIFICATION.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunUntil {
    /// No failover on this node, or a primary that needs no lease (the
    /// paths or operator arbiter, or an operator's override).
    Always,
    /// Not primary.
    Never,
    /// A primary holding a lease, until its end less the margin.
    Until(Instant),
}

impl RunUntil {
    /// Whether guest code may run at `now`.
    pub(crate) fn allows(self, now: Instant) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Until(end) => now < end,
        }
    }
}

/// Failover state on a runtime: a node's inbox, or the witness, and the
/// lease singletons run under.
pub(crate) struct Shared {
    inbox: Mutex<Option<Inbox>>,
    witness: Mutex<Option<Witness>>,
    persist: Mutex<Option<WitnessPersist>>,
    run_until: watch::Sender<RunUntil>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            inbox: Mutex::default(),
            witness: Mutex::default(),
            persist: Mutex::default(),
            run_until: watch::Sender::new(RunUntil::Always),
        }
    }
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl Shared {
    /// Until when singleton instances may run guest code, as it changes.
    pub(crate) fn run_until(&self) -> watch::Receiver<RunUntil> {
        self.run_until.subscribe()
    }

    /// A failover message from node `from`.
    pub(crate) fn deliver(&self, rt: &RuntimeInner, from: u32, msg: FailoverMsg) {
        let decision = {
            let mut witness = self.witness.lock().unwrap_or_else(PoisonError::into_inner);
            witness.as_mut().map(|w| w.handle(from, msg.clone()))
        };
        let reply = match decision {
            None => return self.forward(from, msg),
            Some(None) => return,
            Some(Some(Decision::Answer(reply))) => reply,
            Some(Some(Decision::Grant(grant))) => match self.keep(grant) {
                Some(reply) => reply,
                None => return,
            },
        };
        transport::send(rt, from, Frame::Failover(reply));
    }

    /// Keep `grant` on disk, then apply it (R10): the answer, or `None` if
    /// it could not be kept and was dropped. The witness answers nothing
    /// from the pair meanwhile ([`Witness::handle`]).
    fn keep(&self, grant: Grant) -> Option<FailoverMsg> {
        let persist = self
            .persist
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let kept = persist.map_or(Ok(()), |persist| persist(grant.epoch, grant.promoted));
        let mut witness = self.witness.lock().unwrap_or_else(PoisonError::into_inner);
        let w = witness.as_mut()?;
        match kept {
            Ok(()) => Some(w.apply(grant)),
            Err(e) => {
                w.abandon();
                drop(witness);
                tracing::error!(error = %e, "witness state not kept; grant withheld");
                None
            }
        }
    }

    /// Hand an answer from `from` to this pair node, if `from` is its
    /// witness. Any other member's answer is dropped: a `Granted` from a
    /// node that is not the witness must not promote anyone.
    fn forward(&self, from: u32, msg: FailoverMsg) {
        let (witness, tx) = {
            let inbox = self.inbox.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(inbox) = inbox.as_ref() else {
                return;
            };
            (inbox.witness, inbox.tx.clone())
        };
        if witness == Some(from) {
            let _ = tx.send(msg);
        } else {
            tracing::warn!(
                from,
                ?witness,
                ?msg,
                "failover answer from a node that is not this pair's witness; dropped"
            );
        }
    }
}

// ------------------------------------------------------------------ beats

/// What a node's beats say about it (R1): standing, promoting, epoch, the
/// sender's current stamp, and the latest stamp it has seen from its
/// partner (its echo).
///
/// A primary renews through its partner by echo: it remembers when it set
/// each stamp, and a standby's confirmation echoing stamp `s` renews the
/// lease from when `s` was set. That moment precedes the confirmation
/// however long the beat took, so a held or late beat only ever renews an
/// old lease (`docs/tla/VERIFICATION.md`, L2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Status {
    standing: Standing,
    promoting: bool,
    /// Stepped down to hand over, in a planned switchover.
    yields: bool,
    epoch: u64,
    stamp: u64,
    echo: u64,
}

impl Status {
    fn encode(self) -> Vec<u8> {
        let mut out = vec![
            match self.standing {
                Standing::Primary => 1,
                Standing::Standby => 2,
                Standing::Down => 3,
            },
            // Flags: promoting, then yields. A node that knows only the
            // first reads a yielding node as not promoting, which it is not.
            u8::from(self.promoting) | (u8::from(self.yields) << 1),
        ];
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.stamp.to_le_bytes());
        out.extend_from_slice(&self.echo.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let (&[standing, flags], rest) = bytes.split_first_chunk::<2>()?;
        let (epoch, rest) = rest.split_first_chunk::<8>()?;
        let (stamp, rest) = rest.split_first_chunk::<8>()?;
        let echo: &[u8; 8] = rest.try_into().ok()?;
        Some(Self {
            standing: match standing {
                1 => Standing::Primary,
                2 => Standing::Standby,
                3 => Standing::Down,
                _ => return None,
            },
            promoting: flags & 1 != 0,
            yields: flags & 2 != 0,
            epoch: u64::from_le_bytes(*epoch),
            stamp: u64::from_le_bytes(*stamp),
            echo: u64::from_le_bytes(*echo),
        })
    }
}

/// For the fuzz targets in `fuzz/` only: decode a heartbeat's status
/// payload and, if it is one, encode it again.
#[cfg(fuzzing)]
#[doc(hidden)]
#[must_use]
pub fn fuzz_status(bytes: &[u8]) -> Option<Vec<u8>> {
    Status::decode(bytes).map(Status::encode)
}

/// The partner's latest status on each live path, and whether any path is
/// live.
struct PartnerView {
    /// Live beats: (path, when heard, status).
    live: Vec<(String, Instant, Status)>,
    any_live: bool,
}

impl PartnerView {
    fn read(env: &dyn node::Env, partner: u32) -> Self {
        let beats = env.beats_from(partner);
        let any_live = beats.iter().any(|b| b.live);
        let live = beats
            .into_iter()
            .filter(|b| b.live)
            .filter_map(|b| Some((b.path, b.heard, Status::decode(&b.payload)?)))
            .collect();
        Self { live, any_live }
    }

    /// The partner as a primary on a live path, at its highest epoch.
    fn primary(&self, link_only: bool) -> Option<u64> {
        self.live
            .iter()
            .filter(|(path, _, s)| {
                s.standing == Standing::Primary && (!link_only || path == transport::liveness::LINK)
            })
            .map(|(_, _, s)| s.epoch)
            .max()
    }

    /// The highest stamp of this node's that a live path says the partner,
    /// a standby at no higher epoch than `epoch`, confirms this node's lease
    /// at.
    fn confirmed(&self, epoch: u64) -> Option<u64> {
        self.live
            .iter()
            .filter(|(_, _, s)| s.standing == Standing::Standby && !s.promoting && s.epoch <= epoch)
            .map(|(_, _, s)| s.echo)
            .max()
    }

    /// The partner yields (a planned switchover): a live path says it
    /// stepped down to hand over, and none says it is primary.
    fn yielding(&self) -> bool {
        self.primary(false).is_none()
            && self
                .live
                .iter()
                .any(|(_, _, s)| s.standing == Standing::Down && s.yields)
    }

    /// Whether a live path says the partner is a standby confirming this
    /// node's lease.
    fn confirming_standby(&self) -> bool {
        self.live
            .iter()
            .any(|(_, _, s)| s.standing == Standing::Standby && !s.promoting)
    }

    /// The partner's highest stamp on a live path.
    fn stamp(&self) -> Option<u64> {
        self.live.iter().map(|(_, _, s)| s.stamp).max()
    }
}

impl Runtime {
    /// Take part in failover as one node of an HA pair. Starts `Down`; see
    /// [`crate::failover`]. Call inside the tokio runtime, once.
    ///
    /// # Errors
    ///
    /// [`HostError::Engine`] if failover was already started, or `margin`
    /// is not below `lease`.
    pub fn start_failover(&self, cfg: FailoverConfig) -> Result<FailoverHandle, HostError> {
        if cfg.margin * 2 >= cfg.lease {
            return Err(HostError::Engine(
                "failover margin must be less than half the lease".into(),
            ));
        }
        let rt = self.inner();
        let (inbox_tx, inbox) = mpsc::unbounded_channel();
        {
            let mut slot = rt
                .failover
                .inbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if slot.is_some() {
                return Err(HostError::Engine("failover is already started".into()));
            }
            let witness = match cfg.arbiter {
                Arbiter::Witness(w) => Some(w),
                Arbiter::Paths | Arbiter::Operator => None,
            };
            *slot = Some(Inbox {
                witness,
                tx: inbox_tx,
            });
        }
        rt.failover.run_until.send_replace(RunUntil::Never);
        self.add_peer(cfg.partner);
        if let Arbiter::Witness(witness) = cfg.arbiter {
            self.add_peer(witness);
        }
        let initial = Leadership {
            standing: Standing::Down,
            epoch: cfg.epoch,
            may_run: false,
            awaiting_confirmation: false,
        };
        let (tx, leadership) = watch::channel(initial);
        let node = Node::start(rt, cfg, tx);
        let confirm = Arc::new(Notify::new());
        let (switchover, switchovers) = mpsc::unbounded_channel();
        tokio::spawn(node::run(
            node,
            inbox,
            confirm.clone(),
            switchovers,
            rt.liveness.interval(),
        ));
        Ok(FailoverHandle {
            leadership,
            confirm,
            switchover,
        })
    }

    /// Arbitrate failover for the pair in `cfg` as its witness. This node
    /// runs no apps of that pair. Call inside the tokio runtime, once.
    ///
    /// # Errors
    ///
    /// [`HostError::Engine`] if this node already serves as a witness, or
    /// `first` is not in the pair.
    pub fn serve_witness(&self, cfg: WitnessConfig) -> Result<(), HostError> {
        self.serve_witness_kept(cfg, None)
    }

    /// [`Runtime::serve_witness`], keeping its state with `persist` before
    /// each grant.
    ///
    /// # Errors
    ///
    /// As for [`Runtime::serve_witness`].
    pub fn serve_witness_kept(
        &self,
        cfg: WitnessConfig,
        persist: Option<WitnessPersist>,
    ) -> Result<(), HostError> {
        if !cfg.pair.contains(&cfg.first) {
            return Err(HostError::Engine(format!(
                "node {} is not in the pair {:?}",
                cfg.first, cfg.pair
            )));
        }
        let rt = self.inner();
        let mut slot = rt
            .failover
            .witness
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return Err(HostError::Engine("this node is already a witness".into()));
        }
        for node in cfg.pair {
            self.add_peer(node);
        }
        *rt.failover
            .persist
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = persist;
        *slot = Some(Witness::start(cfg));
        Ok(())
    }
}

#[cfg(test)]
#[path = "failover_tests.rs"]
mod failover_tests;
