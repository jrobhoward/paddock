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
//!   Without the witness nothing is promoted; [`FailoverHandle::confirm`]
//!   lets an operator promote, after waiting out the lease (R7).
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
//! Timing assumption, from S16: message delay plus clock drift stays below
//! twice `margin`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;

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
    /// the witness waits this long after. More than half of the worst
    /// message delay plus clock drift.
    pub margin: Duration,
}

impl FailoverConfig {
    /// A configuration with a 3 s lease and a 500 ms margin.
    #[must_use]
    pub fn new(partner: u32, arbiter: Arbiter) -> Self {
        Self {
            partner,
            arbiter,
            lease: Duration::from_secs(3),
            margin: Duration::from_millis(500),
        }
    }
}

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
    /// The partner looks gone and only an operator's
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
    /// lease from when it sent the request.
    Granted {
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

    /// An operator confirms that the partner is gone: with
    /// [`Arbiter::Operator`], or with a witness that cannot be reached,
    /// the standby is promoted once the partner's lease has run out, and
    /// runs without a lease until it meets its partner or the witness.
    pub fn confirm(&self) {
        self.confirm.notify_one();
    }
}

/// Failover state on a runtime: a node's inbox, or the witness.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    inbox: Mutex<Option<mpsc::UnboundedSender<FailoverMsg>>>,
    witness: Mutex<Option<Witness>>,
}

impl Shared {
    /// A failover message from node `from`.
    pub(crate) fn deliver(&self, rt: &RuntimeInner, from: u32, msg: FailoverMsg) {
        let reply = {
            let mut witness = self.witness.lock().unwrap_or_else(PoisonError::into_inner);
            witness.as_mut().map(|w| w.handle(from, msg.clone()))
        };
        match reply {
            Some(Some(reply)) => {
                transport::send(rt, from, Frame::Failover(reply));
            }
            Some(None) => {}
            None => {
                if let Some(inbox) = self
                    .inbox
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                {
                    let _ = inbox.send(msg);
                }
            }
        }
    }
}

// ------------------------------------------------------------------ beats

/// What a node's beats say about it (R1): standing, promoting, epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Status {
    standing: Standing,
    promoting: bool,
    epoch: u64,
}

impl Status {
    fn encode(self) -> Vec<u8> {
        let mut out = vec![
            match self.standing {
                Standing::Primary => 1,
                Standing::Standby => 2,
                Standing::Down => 3,
            },
            u8::from(self.promoting),
        ];
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let (&[standing, promoting], epoch) = bytes.split_first_chunk::<2>()?;
        Some(Self {
            standing: match standing {
                1 => Standing::Primary,
                2 => Standing::Standby,
                3 => Standing::Down,
                _ => return None,
            },
            promoting: promoting == 1,
            epoch: u64::from_le_bytes(epoch.try_into().ok()?),
        })
    }
}

/// The partner's latest status on each live path, and whether any path is
/// live.
struct PartnerView {
    /// Live beats: (path, when heard, status).
    live: Vec<(String, Instant, Status)>,
    any_live: bool,
}

impl PartnerView {
    fn read(rt: &RuntimeInner, partner: u32) -> Self {
        let beats = rt.liveness.beats_from(partner);
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

    /// The latest time a live path said the partner, a standby at no higher
    /// epoch than `epoch`, confirms this node's lease.
    fn confirmed(&self, epoch: u64) -> Option<Instant> {
        self.live
            .iter()
            .filter(|(_, _, s)| s.standing == Standing::Standby && !s.promoting && s.epoch <= epoch)
            .map(|(_, heard, _)| *heard)
            .max()
    }
}

// ------------------------------------------------------------------ node

struct Node {
    rt: Weak<RuntimeInner>,
    cfg: FailoverConfig,
    standing: Standing,
    epoch: u64,
    /// The lease as this node sees it (already `margin` short).
    lease_until: Option<Instant>,
    /// Running without a lease, by an operator's confirmation.
    overridden: bool,
    promoting: bool,
    /// When this node last confirmed its partner's lease (as a standby);
    /// its start, for a node that does not know.
    confirmed_at: Instant,
    /// The last request to the witness, and when it was sent.
    request_sent: Option<Instant>,
    /// A request is on its way and not yet answered.
    in_flight: bool,
    /// Since when a live path has said the partner is `Down`.
    partner_down_since: Option<Instant>,
    retry_at: Option<Instant>,
    renewals: BTreeMap<u64, Instant>,
    next_seq: u64,
    awaiting: bool,
    confirmed_by_operator: bool,
    tx: watch::Sender<Leadership>,
}

impl Node {
    fn leadership(&self, now: Instant) -> Leadership {
        let leased = match self.cfg.arbiter {
            Arbiter::Witness(_) => self.overridden || self.lease_until.is_some_and(|t| now < t),
            Arbiter::Paths | Arbiter::Operator => true,
        };
        Leadership {
            standing: self.standing,
            epoch: self.epoch,
            may_run: self.standing == Standing::Primary && leased,
            awaiting_confirmation: self.awaiting,
        }
    }

    fn status(&self) -> Status {
        Status {
            standing: self.standing,
            promoting: self.promoting,
            epoch: self.epoch,
        }
    }

    fn publish(&self, rt: &RuntimeInner, now: Instant) {
        rt.liveness.set_payload(self.status().encode());
        let next = self.leadership(now);
        self.tx.send_if_modified(|current| {
            let changed = *current != next;
            *current = next;
            changed
        });
    }

    fn to_witness(&self, rt: &RuntimeInner, msg: FailoverMsg) {
        if let Arbiter::Witness(witness) = self.cfg.arbiter {
            transport::send(rt, witness, Frame::Failover(msg));
        }
    }

    fn step_down(&mut self, epoch: u64, now: Instant) {
        self.standing = Standing::Down;
        self.epoch = self.epoch.max(epoch);
        self.lease_until = None;
        self.overridden = false;
        self.promoting = false;
        self.request_sent = None;
        self.confirmed_at = now;
    }

    fn become_primary(&mut self, epoch: u64, lease_from: Option<Instant>) {
        self.standing = Standing::Primary;
        self.epoch = epoch;
        self.promoting = false;
        self.awaiting = false;
        self.confirmed_by_operator = false;
        self.request_sent = None;
        self.retry_at = None;
        self.lease_until = lease_from.map(|t| t + self.cfg.lease - self.cfg.margin);
    }

    fn extend_lease(&mut self, from: Instant) {
        let until = from + self.cfg.lease - self.cfg.margin;
        self.lease_until = Some(self.lease_until.map_or(until, |t| t.max(until)));
    }

    /// The partner's lease, as far as this node confirmed it, has run out.
    fn waited_out(&self, now: Instant) -> bool {
        now >= self.confirmed_at + self.cfg.lease + self.cfg.margin
    }

    fn handle(&mut self, msg: FailoverMsg, now: Instant) {
        match msg {
            FailoverMsg::LeaseOk { seq } => {
                if let Some(sent) = self.renewals.remove(&seq)
                    && self.standing == Standing::Primary
                {
                    self.extend_lease(sent);
                }
            }
            FailoverMsg::Fenced { epoch } => {
                if self.standing == Standing::Primary && epoch > self.epoch {
                    self.step_down(epoch, now);
                }
            }
            FailoverMsg::Granted { epoch } => {
                self.in_flight = false;
                if let Some(sent) = self.request_sent
                    && self.standing != Standing::Primary
                {
                    self.become_primary(epoch, Some(sent));
                }
            }
            FailoverMsg::NotYet { wait_ms } => {
                self.in_flight = false;
                self.retry_at = Some(now + Duration::from_millis(wait_ms));
            }
            FailoverMsg::Refused { epoch } => {
                self.in_flight = false;
                if self.standing == Standing::Standby {
                    self.step_down(epoch, now);
                } else {
                    self.epoch = self.epoch.max(epoch);
                }
                self.request_sent = None;
                self.retry_at = Some(now + self.cfg.lease);
            }
            FailoverMsg::Renew { .. } | FailoverMsg::Request { .. } => {}
        }
    }

    /// Send a request to the witness unless one is waiting for its retry
    /// time.
    fn request(&mut self, rt: &RuntimeInner, resume: bool, now: Instant) {
        if self.retry_at.is_some_and(|t| now < t) {
            return;
        }
        self.retry_at = Some(now + self.cfg.lease);
        self.request_sent = Some(now);
        self.in_flight = true;
        let ago = now.duration_since(self.confirmed_at);
        self.to_witness(
            rt,
            FailoverMsg::Request {
                resume,
                epoch: self.epoch,
                confirmed_ago_ms: u64::try_from(ago.as_millis()).unwrap_or(u64::MAX),
            },
        );
    }

    fn step(&mut self, rt: &RuntimeInner, now: Instant) {
        let partner = PartnerView::read(rt, self.cfg.partner);
        match self.standing {
            Standing::Primary => self.step_primary(rt, &partner, now),
            Standing::Standby => self.step_standby(rt, &partner, now),
            Standing::Down => self.step_down_node(rt, &partner, now),
        }
    }

    fn step_primary(&mut self, rt: &RuntimeInner, partner: &PartnerView, now: Instant) {
        if let Some(theirs) = partner.primary(false)
            && theirs > self.epoch
        {
            self.step_down(theirs, now);
            return;
        }
        if matches!(self.cfg.arbiter, Arbiter::Witness(_)) {
            if let Some(heard) = partner.confirmed(self.epoch) {
                self.extend_lease(heard);
                if partner.primary(false).is_none() {
                    self.overridden = false;
                }
            }
            let seq = self.next_seq;
            self.next_seq += 1;
            self.renewals.insert(seq, now);
            // Answers older than a lease are of no use.
            let horizon = now.checked_sub(self.cfg.lease).unwrap_or(now);
            self.renewals.retain(|_, sent| *sent >= horizon);
            self.to_witness(
                rt,
                FailoverMsg::Renew {
                    seq,
                    epoch: self.epoch,
                },
            );
        }
    }

    fn step_standby(&mut self, rt: &RuntimeInner, partner: &PartnerView, now: Instant) {
        if let Some(theirs) = partner.primary(false) {
            self.epoch = self.epoch.max(theirs);
        }
        if partner.any_live {
            // The witness keeps no pending request, so with none in flight a
            // standby that hears a primary again may stop asking.
            if self.promoting && !self.in_flight && partner.primary(false).is_some() {
                self.promoting = false;
                self.request_sent = None;
                self.retry_at = None;
            }
            if !self.promoting {
                self.confirmed_at = now;
            }
            self.awaiting = false;
            // Neither node leads: step down, as if restarted, so the node the
            // witness last promoted can resume (or the bootstrap rule runs).
            let partner_down = partner
                .live
                .iter()
                .any(|(_, _, s)| s.standing == Standing::Down);
            let since = if partner_down {
                *self.partner_down_since.get_or_insert(now)
            } else {
                self.partner_down_since = None;
                now
            };
            if partner_down && now.duration_since(since) > self.cfg.lease + self.cfg.margin {
                self.partner_down_since = None;
                self.step_down(self.epoch, now);
            }
            return;
        }
        match self.cfg.arbiter {
            Arbiter::Witness(_) => {
                self.promoting = true;
                self.request(rt, false, now);
                if self.confirmed_by_operator && self.waited_out(now) {
                    self.become_primary(self.epoch + 1, None);
                    self.overridden = true;
                }
            }
            Arbiter::Paths => self.become_primary(self.epoch + 1, None),
            Arbiter::Operator => {
                self.promoting = true;
                self.awaiting = true;
                if self.confirmed_by_operator {
                    self.become_primary(self.epoch + 1, None);
                }
            }
        }
    }

    fn step_down_node(&mut self, rt: &RuntimeInner, partner: &PartnerView, now: Instant) {
        if let Some(theirs) = partner.primary(true)
            && theirs >= self.epoch
        {
            self.standing = Standing::Standby;
            self.epoch = theirs;
            self.request_sent = None;
            self.retry_at = None;
            self.confirmed_at = now;
            return;
        }
        let me = rt.node();
        match self.cfg.arbiter {
            Arbiter::Witness(_) => {
                if self.waited_out(now) {
                    self.request(rt, true, now);
                }
            }
            Arbiter::Paths | Arbiter::Operator => {
                // Bootstrap: with the partner heard and also down, the lower
                // node id leads; heard on no path, `Paths` leads alone.
                let partner_down = partner
                    .live
                    .iter()
                    .any(|(_, _, s)| s.standing == Standing::Down);
                let alone = !partner.any_live && self.cfg.arbiter == Arbiter::Paths;
                if (partner_down && me < self.cfg.partner) || alone {
                    self.become_primary(self.epoch + 1, None);
                }
            }
        }
    }
}

async fn run(
    mut node: Node,
    mut inbox: mpsc::UnboundedReceiver<FailoverMsg>,
    confirm: Arc<Notify>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let now = Instant::now();
        // Wake when the lease runs out, so singletons stop on time (R6).
        let lease_end = node
            .lease_until
            .filter(|t| *t > now && node.standing == Standing::Primary);
        tokio::select! {
            biased;
            msg = inbox.recv() => {
                let Some(msg) = msg else { return };
                node.handle(msg, Instant::now());
            }
            () = confirm.notified() => node.confirmed_by_operator = true,
            () = async { tokio::time::sleep_until(lease_end.unwrap_or(now)).await },
                if lease_end.is_some() => {}
            _ = ticker.tick() => {
                let Some(rt) = node.rt.upgrade() else { return };
                node.step(&rt, Instant::now());
            }
        }
        let Some(rt) = node.rt.upgrade() else { return };
        node.publish(&rt, Instant::now());
    }
}

// ------------------------------------------------------------------ witness

#[derive(Debug)]
struct Witness {
    cfg: WitnessConfig,
    epoch: u64,
    promoted: u32,
    /// The last renewal the witness granted each node.
    renewed: HashMap<u32, Instant>,
}

impl Witness {
    fn partner(&self, node: u32) -> Option<u32> {
        match self.cfg.pair {
            [a, b] if a == node => Some(b),
            [a, b] if b == node => Some(a),
            _ => None,
        }
    }

    fn handle(&mut self, from: u32, msg: FailoverMsg) -> Option<FailoverMsg> {
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
            *slot = Some(inbox_tx);
        }
        self.add_peer(cfg.partner);
        if let Arbiter::Witness(witness) = cfg.arbiter {
            self.add_peer(witness);
        }
        let now = Instant::now();
        let initial = Leadership {
            standing: Standing::Down,
            epoch: 0,
            may_run: false,
            awaiting_confirmation: false,
        };
        let (tx, leadership) = watch::channel(initial);
        let node = Node {
            rt: Arc::downgrade(rt),
            cfg,
            standing: Standing::Down,
            epoch: 0,
            lease_until: None,
            overridden: false,
            promoting: false,
            confirmed_at: now,
            request_sent: None,
            in_flight: false,
            partner_down_since: None,
            retry_at: None,
            renewals: BTreeMap::new(),
            next_seq: 0,
            awaiting: false,
            confirmed_by_operator: false,
            tx,
        };
        node.publish(rt, now);
        let confirm = Arc::new(Notify::new());
        tokio::spawn(run(node, inbox, confirm.clone(), rt.liveness.interval()));
        Ok(FailoverHandle {
            leadership,
            confirm,
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
        *slot = Some(Witness {
            epoch: 1,
            promoted: cfg.first,
            renewed: HashMap::new(),
            cfg,
        });
        Ok(())
    }
}

#[cfg(test)]
#[path = "failover_tests.rs"]
mod failover_tests;
