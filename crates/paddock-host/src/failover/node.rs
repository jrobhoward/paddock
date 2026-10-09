//! A node of the pair as a state machine: one type per standing, holding
//! only what that standing needs, and transitions that consume the current
//! state and return the next.
//!
//! The transitions are named after the actions of the TLA+ model
//! (`docs/tla/FailoverL2.tla`): `fenced`, `stand_down`, `ask`, `cancel`,
//! `rejoin`, `resume`, `granted`, and so on, so a change here can be
//! checked against the model action by action.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;

use super::{Arbiter, FailoverConfig, FailoverMsg, Leadership, PartnerView, Standing, Status};
use crate::runtime::RuntimeInner;
use crate::transport::{self, Frame};

/// A node's standing, with the data that standing has and no other.
#[derive(Debug)]
pub(super) enum State {
    Primary(Primary),
    Standby(Standby),
    Down(Down),
}

/// Runs the singletons while it holds a lease.
#[derive(Debug)]
pub(super) struct Primary {
    epoch: u64,
    /// The lease as this node sees it (already `margin` short).
    lease_until: Option<Instant>,
    /// Running without a lease, by an operator's confirmation.
    overridden: bool,
    /// Renewals sent to the witness and not yet answered, by sequence.
    renewals: BTreeMap<u64, Instant>,
    next_seq: u64,
}

/// Takes over when the primary is gone.
#[derive(Debug)]
pub(super) struct Standby {
    epoch: u64,
    /// Set while it asks to be promoted (it then confirms nothing).
    asking: Option<Asking>,
    /// Since when a live path has said the partner is `Down`.
    partner_down_since: Option<Instant>,
    /// Only an operator's confirmation would promote it.
    awaiting: bool,
}

/// Starting, or stepped down: neither.
#[derive(Debug)]
pub(super) struct Down {
    epoch: u64,
    /// Its request to resume, if any.
    asking: Asking,
}

/// A request to the witness.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Asking {
    /// When the last request was sent.
    sent: Option<Instant>,
    /// Its answer has not come yet.
    in_flight: bool,
    /// When to ask again.
    retry_at: Option<Instant>,
}

/// What every standing shares: configuration, stamps and echoes, the
/// confirmation time, and where leadership is published.
pub(super) struct Ctx {
    rt: Weak<RuntimeInner>,
    cfg: FailoverConfig,
    /// When this node's beats last confirmed its partner's lease: set when
    /// its payload stops confirming (the last moment a confirming beat could
    /// leave); its start, for a node that does not know.
    confirmed_at: Instant,
    /// Whether the payload last published confirms the partner's lease.
    confirming: bool,
    /// This node's stamp (its creation in the high 32 bits), raised every
    /// step, and when each recent one was set.
    stamp: u64,
    stamps: BTreeMap<u64, Instant>,
    /// The partner's highest stamp seen, echoed while confirming.
    seen: u64,
    /// An operator confirmed that the partner is gone.
    confirmed_by_operator: bool,
    tx: watch::Sender<Leadership>,
}

impl State {
    fn standing(&self) -> Standing {
        match self {
            Self::Primary(_) => Standing::Primary,
            Self::Standby(_) => Standing::Standby,
            Self::Down(_) => Standing::Down,
        }
    }

    fn epoch(&self) -> u64 {
        match self {
            Self::Primary(p) => p.epoch,
            Self::Standby(s) => s.epoch,
            Self::Down(d) => d.epoch,
        }
    }

    /// When the lease runs out, for a primary holding one.
    fn lease_end(&self) -> Option<Instant> {
        match self {
            Self::Primary(p) => p.lease_until,
            Self::Standby(_) | Self::Down(_) => None,
        }
    }

    fn leadership(&self, cfg: &FailoverConfig, now: Instant) -> Leadership {
        let may_run = match self {
            Self::Primary(p) => match cfg.arbiter {
                Arbiter::Witness(_) => p.overridden || p.lease_until.is_some_and(|t| now < t),
                Arbiter::Paths | Arbiter::Operator => true,
            },
            Self::Standby(_) | Self::Down(_) => false,
        };
        Leadership {
            standing: self.standing(),
            epoch: self.epoch(),
            may_run,
            awaiting_confirmation: matches!(self, Self::Standby(s) if s.awaiting),
        }
    }

    /// Whether its beats confirm the partner's lease: a standby not asking.
    fn confirming(&self) -> bool {
        matches!(self, Self::Standby(s) if s.asking.is_none())
    }

    fn status(&self, ctx: &Ctx) -> Status {
        Status {
            standing: self.standing(),
            promoting: matches!(self, Self::Standby(s) if s.asking.is_some()),
            epoch: self.epoch(),
            stamp: ctx.stamp,
            echo: if self.confirming() { ctx.seen } else { 0 },
        }
    }

    /// A message from the witness.
    fn on_message(self, ctx: &mut Ctx, msg: FailoverMsg, now: Instant) -> Self {
        match (self, msg) {
            (Self::Primary(p), FailoverMsg::LeaseOk { seq }) => p.lease_ok(ctx, seq),
            (Self::Primary(p), FailoverMsg::Fenced { epoch }) => p.fenced(ctx, epoch, now),
            (Self::Standby(s), FailoverMsg::Granted { epoch }) => s.granted(ctx, epoch),
            (Self::Standby(s), FailoverMsg::NotYet { wait_ms }) => s.not_yet(wait_ms, now),
            (Self::Standby(s), FailoverMsg::Refused { epoch }) => s.refused(ctx, epoch, now),
            (Self::Down(d), FailoverMsg::Granted { epoch }) => d.granted(ctx, epoch),
            (Self::Down(d), FailoverMsg::NotYet { wait_ms }) => d.not_yet(wait_ms, now),
            (Self::Down(d), FailoverMsg::Refused { epoch }) => d.refused(ctx, epoch, now),
            // Anything else is no news to this standing.
            (state, _) => state,
        }
    }

    /// A periodic step: look at the partner, renew, ask, or move on.
    fn step(self, ctx: &mut Ctx, rt: &RuntimeInner, now: Instant) -> Self {
        ctx.stamp += 1;
        ctx.stamps.insert(ctx.stamp, now);
        // Stamps older than a lease cannot renew anything.
        let horizon = now.checked_sub(ctx.cfg.lease).unwrap_or(now);
        ctx.stamps.retain(|_, set| *set >= horizon);
        let partner = PartnerView::read(rt, ctx.cfg.partner);
        if let Some(stamp) = partner.stamp() {
            ctx.seen = ctx.seen.max(stamp);
        }
        match self {
            Self::Primary(p) => p.step(ctx, rt, &partner, now),
            Self::Standby(s) => s.step(ctx, rt, &partner, now),
            Self::Down(d) => d.step(ctx, rt, &partner, now),
        }
    }
}

impl Ctx {
    fn to_witness(&self, rt: &RuntimeInner, msg: FailoverMsg) {
        if let Arbiter::Witness(witness) = self.cfg.arbiter {
            transport::send(rt, witness, Frame::Failover(msg));
        }
    }

    /// The partner's lease, as far as this node confirmed it, has run out.
    fn waited_out(&self, now: Instant) -> bool {
        now >= self.confirmed_at + self.cfg.lease + self.cfg.margin
    }

    /// Step down to `Down` at `epoch`.
    fn step_down(&mut self, epoch: u64, now: Instant) -> State {
        State::Down(self.down(epoch, now))
    }

    fn down(&mut self, epoch: u64, now: Instant) -> Down {
        self.confirmed_at = now;
        Down {
            epoch,
            asking: Asking::default(),
        }
    }

    /// Become primary at `epoch`, with a lease from `lease_from` (a grant)
    /// or none (`Paths`, `Operator`, or an override).
    fn promote(&mut self, epoch: u64, lease_from: Option<Instant>, overridden: bool) -> State {
        self.confirmed_by_operator = false;
        State::Primary(Primary {
            epoch,
            lease_until: lease_from.map(|t| t + self.cfg.lease - self.cfg.margin),
            overridden,
            renewals: BTreeMap::new(),
            next_seq: 0,
        })
    }

    /// Send a request to the witness unless one waits for its retry time.
    fn ask(
        &mut self,
        rt: &RuntimeInner,
        asking: &mut Asking,
        resume: bool,
        epoch: u64,
        now: Instant,
    ) {
        if asking.retry_at.is_some_and(|t| now < t) {
            return;
        }
        if self.confirming {
            // The payload stops confirming on this step; no confirming beat
            // leaves after it is published, a moment from now.
            self.confirmed_at = now;
        }
        *asking = Asking {
            sent: Some(now),
            in_flight: true,
            retry_at: Some(now + self.cfg.lease),
        };
        let ago = now.duration_since(self.confirmed_at);
        self.to_witness(
            rt,
            FailoverMsg::Request {
                resume,
                epoch,
                confirmed_ago_ms: u64::try_from(ago.as_millis()).unwrap_or(u64::MAX),
            },
        );
    }
}

impl Asking {
    fn not_yet(self, wait_ms: u64, now: Instant) -> Self {
        Self {
            in_flight: false,
            retry_at: Some(now + Duration::from_millis(wait_ms)),
            ..self
        }
    }
}

impl Primary {
    fn extend(&mut self, cfg: &FailoverConfig, from: Instant) {
        let until = from + cfg.lease - cfg.margin;
        self.lease_until = Some(self.lease_until.map_or(until, |t| t.max(until)));
    }

    /// `LeaseOk`: the witness renewed the lease sent at `seq`.
    fn lease_ok(mut self, ctx: &Ctx, seq: u64) -> State {
        if let Some(sent) = self.renewals.remove(&seq) {
            self.extend(&ctx.cfg, sent);
        }
        State::Primary(self)
    }

    /// `Fenced`: the witness's epoch is higher.
    fn fenced(self, ctx: &mut Ctx, epoch: u64, now: Instant) -> State {
        if epoch > self.epoch {
            ctx.step_down(epoch, now)
        } else {
            State::Primary(self)
        }
    }

    fn step(
        mut self,
        ctx: &mut Ctx,
        rt: &RuntimeInner,
        partner: &PartnerView,
        now: Instant,
    ) -> State {
        // `Fenced` by the partner: it is primary at a higher epoch.
        if let Some(theirs) = partner.primary(false)
            && theirs > self.epoch
        {
            return ctx.step_down(theirs, now);
        }
        if matches!(ctx.cfg.arbiter, Arbiter::Witness(_)) {
            // Renewal through the partner, by echo.
            if let Some(set) = partner
                .confirmed(self.epoch)
                .and_then(|echo| ctx.stamps.get(&echo).copied())
            {
                self.extend(&ctx.cfg, set);
                if partner.primary(false).is_none() {
                    self.overridden = false;
                }
            }
            // Renewal at the witness, by send time.
            let seq = self.next_seq;
            self.next_seq += 1;
            self.renewals.insert(seq, now);
            // Answers older than a lease are of no use.
            let horizon = now.checked_sub(ctx.cfg.lease).unwrap_or(now);
            self.renewals.retain(|_, sent| *sent >= horizon);
            ctx.to_witness(
                rt,
                FailoverMsg::Renew {
                    seq,
                    epoch: self.epoch,
                },
            );
        }
        State::Primary(self)
    }
}

impl Standby {
    /// `Granted`: promoted, with a lease from when the request left.
    fn granted(self, ctx: &mut Ctx, epoch: u64) -> State {
        match self.asking.and_then(|a| a.sent) {
            Some(sent) => ctx.promote(epoch, Some(sent), false),
            None => State::Standby(self),
        }
    }

    fn not_yet(mut self, wait_ms: u64, now: Instant) -> State {
        self.asking = self.asking.map(|a| a.not_yet(wait_ms, now));
        State::Standby(self)
    }

    /// `Refused`: someone was promoted at a higher epoch; step down.
    fn refused(self, ctx: &mut Ctx, epoch: u64, now: Instant) -> State {
        let mut down = ctx.down(self.epoch.max(epoch), now);
        down.asking.retry_at = Some(now + ctx.cfg.lease);
        State::Down(down)
    }

    fn step(
        mut self,
        ctx: &mut Ctx,
        rt: &RuntimeInner,
        partner: &PartnerView,
        now: Instant,
    ) -> State {
        if let Some(theirs) = partner.primary(false) {
            self.epoch = self.epoch.max(theirs);
        }
        if partner.any_live {
            self.awaiting = false;
            // `Cancel`: the witness keeps no pending request, so with none in
            // flight a standby that hears a primary again stops asking.
            if self.asking.is_some_and(|a| !a.in_flight) && partner.primary(false).is_some() {
                self.asking = None;
            }
            // `StandDown`: neither node leads; step down, as if restarted, so
            // the node the witness last promoted can resume (or the
            // bootstrap rule runs).
            let partner_down = partner
                .live
                .iter()
                .any(|(_, _, s)| s.standing == Standing::Down);
            if !partner_down {
                self.partner_down_since = None;
            } else if let Some(since) = self.partner_down_since {
                if now.duration_since(since) > ctx.cfg.lease + ctx.cfg.margin {
                    return ctx.step_down(self.epoch, now);
                }
            } else {
                self.partner_down_since = Some(now);
            }
            return State::Standby(self);
        }
        // The partner is silent on every path.
        match ctx.cfg.arbiter {
            Arbiter::Witness(_) => {
                // `Ask`.
                let mut asking = self.asking.unwrap_or_default();
                ctx.ask(rt, &mut asking, false, self.epoch, now);
                self.asking = Some(asking);
                // An operator's override, once the partner's lease is out.
                if ctx.confirmed_by_operator && ctx.waited_out(now) {
                    return ctx.promote(self.epoch + 1, None, true);
                }
                State::Standby(self)
            }
            Arbiter::Paths => ctx.promote(self.epoch + 1, None, false),
            Arbiter::Operator => {
                if ctx.confirmed_by_operator {
                    return ctx.promote(self.epoch + 1, None, false);
                }
                self.asking = Some(self.asking.unwrap_or_default());
                self.awaiting = true;
                State::Standby(self)
            }
        }
    }
}

impl Down {
    /// `Granted`: resumed as primary.
    fn granted(self, ctx: &mut Ctx, epoch: u64) -> State {
        match self.asking.sent {
            Some(sent) => ctx.promote(epoch, Some(sent), false),
            None => State::Down(self),
        }
    }

    fn not_yet(mut self, wait_ms: u64, now: Instant) -> State {
        self.asking = self.asking.not_yet(wait_ms, now);
        State::Down(self)
    }

    /// `Refused`: not the node the witness last promoted; wait.
    fn refused(mut self, ctx: &Ctx, epoch: u64, now: Instant) -> State {
        self.epoch = self.epoch.max(epoch);
        self.asking = Asking {
            sent: None,
            in_flight: false,
            retry_at: Some(now + ctx.cfg.lease),
        };
        State::Down(self)
    }

    fn step(
        mut self,
        ctx: &mut Ctx,
        rt: &RuntimeInner,
        partner: &PartnerView,
        now: Instant,
    ) -> State {
        // `Rejoin`: a primary on the link, at no lower an epoch.
        if let Some(theirs) = partner.primary(true)
            && theirs >= self.epoch
        {
            ctx.confirmed_at = now;
            return State::Standby(Standby {
                epoch: theirs,
                asking: None,
                partner_down_since: None,
                awaiting: false,
            });
        }
        match ctx.cfg.arbiter {
            Arbiter::Witness(_) => {
                // `Resume`, once its own confirmations are out of date.
                if ctx.waited_out(now) {
                    ctx.ask(rt, &mut self.asking, true, self.epoch, now);
                }
                State::Down(self)
            }
            Arbiter::Paths | Arbiter::Operator => {
                // Bootstrap: with the partner heard and also down, the lower
                // node id leads; heard on no path, `Paths` leads alone.
                let partner_down = partner
                    .live
                    .iter()
                    .any(|(_, _, s)| s.standing == Standing::Down);
                let alone = !partner.any_live && ctx.cfg.arbiter == Arbiter::Paths;
                if (partner_down && rt.node() < ctx.cfg.partner) || alone {
                    ctx.promote(self.epoch + 1, None, false)
                } else {
                    State::Down(self)
                }
            }
        }
    }
}

/// A node's failover, between steps.
pub(super) struct Node {
    state: State,
    ctx: Ctx,
}

impl Node {
    /// A node starting `Down` at epoch 0.
    pub(super) fn start(
        rt: &Arc<RuntimeInner>,
        cfg: FailoverConfig,
        tx: watch::Sender<Leadership>,
    ) -> Self {
        let now = Instant::now();
        let mut node = Self {
            state: State::Down(Down {
                epoch: 0,
                asking: Asking::default(),
            }),
            ctx: Ctx {
                rt: Arc::downgrade(rt),
                cfg,
                confirmed_at: now,
                confirming: false,
                stamp: u64::from(rt.creation()) << 32,
                stamps: BTreeMap::new(),
                seen: 0,
                confirmed_by_operator: false,
                tx,
            },
        };
        node.publish(rt, now);
        node
    }

    /// Apply a transition to the current state.
    fn apply(&mut self, transition: impl FnOnce(State, &mut Ctx) -> State) {
        // A placeholder for the moment the state is moved out.
        let placeholder = State::Down(Down {
            epoch: 0,
            asking: Asking::default(),
        });
        let state = std::mem::replace(&mut self.state, placeholder);
        self.state = transition(state, &mut self.ctx);
    }

    fn publish(&mut self, rt: &RuntimeInner, now: Instant) {
        let status = self.state.status(&self.ctx);
        rt.liveness.set_payload(status.encode());
        let confirming = self.state.confirming();
        if self.ctx.confirming && !confirming {
            // No confirming beat leaves after this.
            self.ctx.confirmed_at = now;
        }
        self.ctx.confirming = confirming;
        let next = self.state.leadership(&self.ctx.cfg, now);
        self.ctx.tx.send_if_modified(|current| {
            let changed = *current != next;
            *current = next;
            changed
        });
    }
}

/// Step, handle the witness's answers and an operator's confirmation, and
/// publish after each, until the runtime is gone.
pub(super) async fn run(
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
        let lease_end = node.state.lease_end().filter(|t| *t > now);
        tokio::select! {
            biased;
            msg = inbox.recv() => {
                let Some(msg) = msg else { return };
                let now = Instant::now();
                node.apply(|state, ctx| state.on_message(ctx, msg, now));
            }
            () = confirm.notified() => node.ctx.confirmed_by_operator = true,
            () = async { tokio::time::sleep_until(lease_end.unwrap_or(now)).await },
                if lease_end.is_some() => {}
            _ = ticker.tick() => {
                let Some(rt) = node.ctx.rt.upgrade() else { return };
                let now = Instant::now();
                node.apply(|state, ctx| state.step(ctx, &rt, now));
            }
        }
        let Some(rt) = node.ctx.rt.upgrade() else {
            return;
        };
        node.publish(&rt, Instant::now());
    }
}
