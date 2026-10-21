//! A node of the pair as a state machine: one type per standing, holding
//! only what that standing needs, and transitions that consume the current
//! state and return the next.
//!
//! The transitions are named after the actions of the TLA+ model
//! (`docs/tla/FailoverL2.tla`): `fenced`, `stand_down`, `ask`, `cancel`,
//! `rejoin`, `resume`, `granted`, `switchover`, and so on, so a change
//! here can be checked against the model action by action. `record` and
//! `recorded` are spike S20's `GoAlone` and `ReportSynced` at the witness
//! (`spikes/s20/PairReplication.tla`).

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::Instant;

use super::{
    Arbiter, FailoverConfig, FailoverMsg, Leadership, PartnerView, RecordAsk, RecordRefused,
    RunUntil, Standing, Status, SwitchoverAsk, SwitchoverRefused,
};
use crate::runtime::RuntimeInner;
use crate::transport::liveness::Beat;
use crate::transport::{self, Frame, SendError};

/// What the transitions see of the node: liveness and the transport. The
/// runtime in a node; a fake in the unit tests, which drive transitions
/// one at a time.
pub(super) trait Env {
    /// This node's id.
    fn node(&self) -> u32;
    /// The last beat from `node` on each path.
    fn beats_from(&self, node: u32) -> Vec<Beat>;
    /// Whether frames to `node` are sent (it hears this node).
    fn reachable(&self, node: u32) -> bool;
    /// Send `msg` to `to`.
    fn send(&self, to: u32, msg: FailoverMsg);
}

impl Env for RuntimeInner {
    fn node(&self) -> u32 {
        RuntimeInner::node(self)
    }

    fn beats_from(&self, node: u32) -> Vec<Beat> {
        self.liveness.beats_from(node)
    }

    fn reachable(&self, node: u32) -> bool {
        self.liveness.reachable(node)
    }

    fn send(&self, to: u32, msg: FailoverMsg) {
        transport::send(self, to, Frame::Failover(msg));
    }
}

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
    /// The witness has recorded the partner out of date (R13).
    partner_outdated: bool,
    /// Changes to the record sent to the witness and not yet answered, by
    /// sequence: when each left, and who waits for it. Dropped, and so
    /// answered `NoAnswer`, a lease after they left or when this node
    /// stops being primary.
    records: BTreeMap<u64, (Instant, oneshot::Sender<Result<(), RecordRefused>>)>,
}

/// Takes over when the primary is gone.
#[derive(Debug)]
pub(super) struct Standby {
    epoch: u64,
    /// Set while it asks to be promoted (it then confirms nothing).
    asking: Option<Asking>,
    /// Since when a live path has said the partner is `Down`.
    partner_down_since: Option<Instant>,
    /// Only an operator's confirmation would promote it: the partner is
    /// silent on every path and the witness cannot be reached.
    awaiting: bool,
}

/// Starting, or stepped down: neither.
#[derive(Debug)]
pub(super) struct Down {
    epoch: u64,
    /// Its request to resume, if any.
    asking: Asking,
    /// Stepped down to hand over (a planned switchover), until it rejoins
    /// or resumes; its beats say so.
    yielding: bool,
}

/// A request to the witness.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Asking {
    /// When the last request was sent (a grant is taken while set).
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
    /// When each request sent this episode left, by sequence: a grant's
    /// lease runs from its own request's send time.
    requests: BTreeMap<u64, Instant>,
    next_request: u64,
    /// An operator confirmed that the partner is gone, when asked.
    confirmed_by_operator: bool,
    /// The last lease end logged as noticed late.
    late_logged: Option<Instant>,
    /// The last lease end logged as extended, for this term.
    extended_logged: Option<Instant>,
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

    /// Until when singletons may run guest code: as `may_run` in
    /// [`State::leadership`], but as a deadline instances check themselves.
    fn run_until(&self, cfg: &FailoverConfig) -> RunUntil {
        match self {
            Self::Primary(p) => match cfg.arbiter {
                Arbiter::Witness(_) if !p.overridden => {
                    p.lease_until.map_or(RunUntil::Never, RunUntil::Until)
                }
                Arbiter::Witness(_) | Arbiter::Paths | Arbiter::Operator => RunUntil::Always,
            },
            Self::Standby(_) | Self::Down(_) => RunUntil::Never,
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
            partner_outdated: matches!(self, Self::Primary(p) if p.partner_outdated),
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
            yields: matches!(self, Self::Down(d) if d.yielding),
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
            (Self::Primary(p), FailoverMsg::Recorded { seq, outdated }) => {
                p.recorded(seq, outdated)
            }
            (
                Self::Standby(s),
                FailoverMsg::Granted {
                    seq,
                    epoch,
                    outdated,
                },
            ) => s.granted(ctx, seq, epoch, outdated),
            (Self::Standby(s), FailoverMsg::NotYet { wait_ms }) => s.not_yet(wait_ms, now),
            (Self::Standby(s), FailoverMsg::Refused { epoch }) => s.refused(ctx, epoch, now),
            (
                Self::Down(d),
                FailoverMsg::Granted {
                    seq,
                    epoch,
                    outdated,
                },
            ) => d.granted(ctx, seq, epoch, outdated),
            (Self::Down(d), FailoverMsg::NotYet { wait_ms }) => d.not_yet(wait_ms, now),
            (Self::Down(d), FailoverMsg::Refused { epoch }) => d.refused(ctx, epoch, now),
            // Anything else is no news to this standing.
            (state, _) => state,
        }
    }

    /// A periodic step: look at the partner, renew, ask, or move on.
    fn step(self, ctx: &mut Ctx, env: &dyn Env, now: Instant) -> Self {
        ctx.stamp += 1;
        ctx.stamps.insert(ctx.stamp, now);
        // Stamps older than a lease cannot renew anything.
        let horizon = now.checked_sub(ctx.cfg.lease).unwrap_or(now);
        ctx.stamps.retain(|_, set| *set >= horizon);
        let partner = PartnerView::read(env, ctx.cfg.partner);
        if let Some(stamp) = partner.stamp() {
            ctx.seen = ctx.seen.max(stamp);
        }
        match self {
            Self::Primary(p) => p.step(ctx, env, &partner, now),
            Self::Standby(s) => s.step(ctx, env, &partner, now),
            Self::Down(d) => d.step(ctx, env, &partner, now),
        }
    }
}

impl Ctx {
    fn to_witness(&self, env: &dyn Env, msg: FailoverMsg) {
        if let Arbiter::Witness(witness) = self.cfg.arbiter {
            env.send(witness, msg);
        }
    }

    /// Whether the witness is heard and hears this node. Without a witness
    /// arbiter, false.
    fn witness_reachable(&self, env: &dyn Env) -> bool {
        let Arbiter::Witness(witness) = self.cfg.arbiter else {
            return false;
        };
        env.reachable(witness) && env.beats_from(witness).iter().any(|b| b.live)
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
        self.confirmed_by_operator = false;
        self.requests.clear();
        Down {
            epoch,
            asking: Asking::default(),
            yielding: false,
        }
    }

    /// Become primary at `epoch`, with a lease from `lease_from` (a grant)
    /// or none (`Paths`, `Operator`, or an override), its partner recorded
    /// out of date or not.
    fn promote(
        &mut self,
        epoch: u64,
        lease_from: Option<Instant>,
        overridden: bool,
        partner_outdated: bool,
    ) -> State {
        self.confirmed_by_operator = false;
        self.requests.clear();
        State::Primary(Primary {
            epoch,
            lease_until: lease_from.map(|t| t + self.cfg.lease - self.cfg.margin),
            overridden,
            renewals: BTreeMap::new(),
            next_seq: 0,
            partner_outdated,
            records: BTreeMap::new(),
        })
    }

    /// Send a request to the witness unless one waits for its retry time.
    fn ask(&mut self, env: &dyn Env, asking: &mut Asking, resume: bool, epoch: u64, now: Instant) {
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
        let seq = self.next_request;
        self.next_request += 1;
        self.requests.insert(seq, now);
        // A grant for a request older than a lease would start a lease
        // already over.
        let horizon = now.checked_sub(self.cfg.lease).unwrap_or(now);
        self.requests.retain(|_, sent| *sent >= horizon);
        let ago = now.duration_since(self.confirmed_at);
        self.to_witness(
            env,
            FailoverMsg::Request {
                seq,
                resume,
                epoch,
                confirmed_ago_ms: u64::try_from(ago.as_millis()).unwrap_or(u64::MAX),
            },
        );
    }

    /// When request `seq` left, if a grant for it is taken: this episode's
    /// requests, while asking, and not older than a lease. A grant arriving
    /// after a retry starts the lease from its own request, not the retry,
    /// so no bound on the witness's answer time is needed.
    fn granted_request(&self, asking: Option<&Asking>, seq: u64) -> Option<Instant> {
        asking.and_then(|a| a.sent)?;
        self.requests.get(&seq).copied()
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

    /// `Recorded`: the witness keeps the record asked for in `seq`.
    fn recorded(mut self, seq: u64, outdated: bool) -> State {
        if let Some((_, reply)) = self.records.remove(&seq) {
            self.partner_outdated = outdated;
            let _ = reply.send(Ok(()));
        }
        State::Primary(self)
    }

    /// `Record`: ask the witness to set or clear the record, at this
    /// primary's epoch.
    fn record(&mut self, ctx: &Ctx, env: &dyn Env, ask: RecordAsk, now: Instant) {
        if ask.epoch != self.epoch || self.overridden {
            // An overridden primary is unknown to the witness.
            let _ = ask.reply.send(Err(RecordRefused::NotPrimary));
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.records.insert(seq, (now, ask.reply));
        ctx.to_witness(
            env,
            FailoverMsg::Record {
                seq,
                epoch: self.epoch,
                outdated: ask.outdated,
            },
        );
    }

    /// `Fenced`: the witness's epoch is higher.
    fn fenced(self, ctx: &mut Ctx, epoch: u64, now: Instant) -> State {
        if epoch > self.epoch {
            ctx.step_down(epoch, now)
        } else {
            State::Primary(self)
        }
    }

    fn step(mut self, ctx: &mut Ctx, env: &dyn Env, partner: &PartnerView, now: Instant) -> State {
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
            self.records.retain(|_, (sent, _)| *sent >= horizon);
            ctx.to_witness(
                env,
                FailoverMsg::Renew {
                    seq,
                    epoch: self.epoch,
                },
            );
        }
        State::Primary(self)
    }
}

impl Primary {
    /// `Switchover`: hand over on purpose. Step down at once, yielding; ask
    /// to resume only a lease later than a down node would, so the
    /// standby's request reaches the witness first.
    fn switchover(self, ctx: &mut Ctx, now: Instant) -> State {
        let mut down = ctx.down(self.epoch, now);
        down.yielding = true;
        down.asking.retry_at = Some(now + ctx.cfg.lease + ctx.cfg.lease + ctx.cfg.margin);
        State::Down(down)
    }
}

impl Standby {
    /// `Granted`: promoted, with a lease from when the granted request
    /// left.
    fn granted(self, ctx: &mut Ctx, seq: u64, epoch: u64, outdated: bool) -> State {
        match ctx.granted_request(self.asking.as_ref(), seq) {
            Some(sent) => ctx.promote(epoch, Some(sent), false, outdated),
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

    fn step(mut self, ctx: &mut Ctx, env: &dyn Env, partner: &PartnerView, now: Instant) -> State {
        if let Some(theirs) = partner.primary(false) {
            self.epoch = self.epoch.max(theirs);
        }
        if partner.any_live && partner.yielding() && matches!(ctx.cfg.arbiter, Arbiter::Witness(_))
        {
            // The partner yields (a planned switchover): `Ask` at once, as
            // on silence, and do not stand down for its `Down`.
            self.awaiting = false;
            ctx.confirmed_by_operator = false;
            self.partner_down_since = None;
            let mut asking = self.asking.unwrap_or_default();
            ctx.ask(env, &mut asking, false, self.epoch, now);
            self.asking = Some(asking);
            return State::Standby(self);
        }
        if partner.any_live {
            self.awaiting = false;
            // An operator's confirmation is about this silence only; a
            // stale one would promote on a later, unrelated cut.
            // `Cancel`: the witness keeps no pending request, so with none in
            // flight a standby that hears a primary again stops asking. A
            // request past its retry time counts as answered: its answer
            // was lost, and waiting for it would keep this standby from
            // confirming the primary's lease for good. A grant arriving
            // after the cancel finds no request and is ignored.
            if self
                .asking
                .is_some_and(|a| !a.in_flight || a.retry_at.is_some_and(|t| now >= t))
                && partner.primary(false).is_some()
            {
                self.asking = None;
                ctx.requests.clear();
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
                ctx.ask(env, &mut asking, false, self.epoch, now);
                self.asking = Some(asking);
                // An operator is asked only while the witness cannot be
                // reached either. A confirmation, once given, stands even
                // if the witness is reached again before it takes effect:
                // the operator's word is final.
                self.awaiting = !ctx.confirmed_by_operator && !ctx.witness_reachable(env);
                // `Override`, once the partner's lease is out.
                if ctx.confirmed_by_operator && ctx.waited_out(now) {
                    return ctx.promote(self.epoch + 1, None, true, false);
                }
                State::Standby(self)
            }
            Arbiter::Paths => ctx.promote(self.epoch + 1, None, false, false),
            Arbiter::Operator => {
                if ctx.confirmed_by_operator {
                    return ctx.promote(self.epoch + 1, None, false, false);
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
    fn granted(self, ctx: &mut Ctx, seq: u64, epoch: u64, outdated: bool) -> State {
        match ctx.granted_request(Some(&self.asking), seq) {
            Some(sent) => ctx.promote(epoch, Some(sent), false, outdated),
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

    fn step(mut self, ctx: &mut Ctx, env: &dyn Env, partner: &PartnerView, now: Instant) -> State {
        // `Rejoin`: a primary on the link, at no lower an epoch.
        if let Some(theirs) = partner.primary(true)
            && theirs >= self.epoch
        {
            ctx.confirmed_at = now;
            ctx.confirmed_by_operator = false;
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
                    ctx.ask(env, &mut self.asking, true, self.epoch, now);
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
                if (partner_down && env.node() < ctx.cfg.partner) || alone {
                    ctx.promote(self.epoch + 1, None, false, false)
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
                epoch: cfg.epoch,
                asking: Asking::default(),
                yielding: false,
            }),
            ctx: Ctx {
                rt: Arc::downgrade(rt),
                cfg,
                confirmed_at: now,
                confirming: false,
                stamp: u64::from(rt.creation()) << 32,
                stamps: BTreeMap::new(),
                seen: 0,
                requests: BTreeMap::new(),
                next_request: 0,
                confirmed_by_operator: false,
                late_logged: None,
                extended_logged: None,
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
            yielding: false,
        });
        let state = std::mem::replace(&mut self.state, placeholder);
        self.state = transition(state, &mut self.ctx);
    }

    /// Log a lease that ended well before this task noticed (the process
    /// was paused or starved), once per lease end, with how long ago it
    /// ended. Singleton instances stopped running guest code at the end
    /// themselves; the gate's kill comes only now. `scripts/soak.sh` reads
    /// this record (`failover lease ended`, `ended_ms_ago`).
    fn log_late_lease_end(&mut self, rt: &RuntimeInner, now: Instant) {
        let current = *rt.failover.run_until.borrow();
        let RunUntil::Until(end) = current else {
            return;
        };
        if now <= end || self.ctx.late_logged == Some(end) {
            return;
        }
        self.ctx.late_logged = Some(end);
        let ago = now - end;
        if ago > rt.liveness.interval() {
            tracing::warn!(
                ended_ms_ago = u64::try_from(ago.as_millis()).unwrap_or(u64::MAX),
                "failover lease ended before this node noticed; singleton instances \
                 stopped running guest code at its end"
            );
        }
    }

    /// Log the lease's end (debug, target `paddock::lease`) whenever it
    /// moves more than `margin` past the end last logged, so the logs bound
    /// every lease end: it is never later than the last record's end plus
    /// its `slack_ms`, even when the process is killed before it can log
    /// anything more. `scripts/soak.sh` reads these records.
    fn log_extension(&mut self, run_until: RunUntil, now: Instant) {
        let RunUntil::Until(end) = run_until else {
            self.ctx.extended_logged = None;
            return;
        };
        let slack = self.ctx.cfg.margin;
        if self
            .ctx
            .extended_logged
            .is_some_and(|logged| end <= logged + slack)
        {
            return;
        }
        self.ctx.extended_logged = Some(end);
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        tracing::debug!(
            target: "paddock::lease",
            until_ms = ms(end.saturating_duration_since(now)),
            slack_ms = ms(slack),
            "failover lease extended"
        );
    }

    /// An operator's confirmation: taken only while this node awaits one,
    /// and then kept until it takes effect or the partner is heard again.
    fn confirm(&mut self) {
        if matches!(&self.state, State::Standby(s) if s.awaiting) {
            tracing::warn!("failover: operator confirmed the partner is gone");
            self.ctx.confirmed_by_operator = true;
        } else {
            tracing::info!(
                "failover: operator confirmation ignored; this node was not waiting for one"
            );
        }
    }

    /// A planned switchover, if this node is a primary with a lease whose
    /// partner a live path shows as a confirming standby; with
    /// `check_only`, only whether it would.
    fn switchover(
        &mut self,
        env: &dyn Env,
        now: Instant,
        check_only: bool,
    ) -> Result<(), SwitchoverRefused> {
        if !matches!(self.ctx.cfg.arbiter, Arbiter::Witness(_)) {
            return Err(SwitchoverRefused::NoWitness);
        }
        match &self.state {
            State::Primary(p) if p.overridden => return Err(SwitchoverRefused::Overridden),
            State::Primary(p) if p.partner_outdated => {
                return Err(SwitchoverRefused::PartnerOutdated);
            }
            State::Primary(_) => {}
            State::Standby(_) | State::Down(_) => return Err(SwitchoverRefused::NotPrimary),
        }
        if !PartnerView::read(env, self.ctx.cfg.partner).confirming_standby() {
            return Err(SwitchoverRefused::NoStandby);
        }
        if check_only {
            return Ok(());
        }
        tracing::warn!(
            epoch = self.state.epoch(),
            "failover: planned switchover; handing over"
        );
        self.apply(|state, ctx| match state {
            State::Primary(p) => p.switchover(ctx, now),
            other => other,
        });
        Ok(())
    }

    /// A change to the witness's record, asked by the replication layer.
    fn record(&mut self, env: &dyn Env, ask: RecordAsk, now: Instant) {
        match &mut self.state {
            State::Primary(p) if matches!(self.ctx.cfg.arbiter, Arbiter::Witness(_)) => {
                p.record(&self.ctx, env, ask, now);
            }
            _ => {
                let _ = ask.reply.send(Err(RecordRefused::NotPrimary));
            }
        }
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
        self.log_late_lease_end(rt, now);
        // Before the leadership, so a singleton the gate starts on it finds
        // its lease already set.
        let run_until = self.state.run_until(&self.ctx.cfg);
        rt.failover
            .run_until
            .send_if_modified(|current| std::mem::replace(current, run_until) != run_until);
        self.log_extension(run_until, now);
        let next = self.state.leadership(&self.ctx.cfg, now);
        self.ctx.tx.send_if_modified(|current| {
            let changed = *current != next;
            *current = next;
            changed
        });
    }
}

/// What the failover task takes besides its steps.
pub(super) struct Inputs {
    /// The witness's answers.
    pub(super) inbox: mpsc::UnboundedReceiver<FailoverMsg>,
    /// An operator's confirmation.
    pub(super) confirm: Arc<Notify>,
    /// Planned switchovers.
    pub(super) switchovers: mpsc::UnboundedReceiver<SwitchoverAsk>,
    /// Changes to the witness's record, from the replication layer.
    pub(super) records: mpsc::UnboundedReceiver<RecordAsk>,
}

/// Step, handle the witness's answers, an operator's confirmation, planned
/// switchovers and changes to the record, and publish after each, until
/// the runtime is gone.
pub(super) async fn run(mut node: Node, inputs: Inputs, interval: Duration) {
    let Inputs {
        mut inbox,
        confirm,
        mut switchovers,
        mut records,
    } = inputs;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // A planned switchover's `Release`, sent once the step down is
        // published (R14).
        let mut release = None;
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
            () = confirm.notified() => node.confirm(),
            Some(ask) = switchovers.recv() => {
                let Some(rt) = node.ctx.rt.upgrade() else { return };
                let epoch = node.state.epoch();
                let done = node.switchover(&*rt, Instant::now(), ask.check_only);
                if done.is_ok() && !ask.check_only {
                    release = Some(epoch);
                }
                let _ = ask.reply.send(done);
            }
            Some(ask) = records.recv() => {
                let Some(rt) = node.ctx.rt.upgrade() else { return };
                node.record(&*rt, ask, Instant::now());
            }
            () = async { tokio::time::sleep_until(lease_end.unwrap_or(now)).await },
                if lease_end.is_some() => {}
            _ = ticker.tick() => {
                let Some(rt) = node.ctx.rt.upgrade() else { return };
                let now = Instant::now();
                node.apply(|state, ctx| state.step(ctx, &*rt, now));
            }
        }
        let Some(rt) = node.ctx.rt.upgrade() else {
            return;
        };
        node.publish(&rt, Instant::now());
        if let (Some(epoch), Arbiter::Witness(witness)) = (release, node.ctx.cfg.arbiter) {
            // A witness one release older cannot read a release; it then
            // waits the lease out, as for a release lost.
            let release = Frame::Failover(FailoverMsg::Release { epoch });
            if transport::try_send(&rt, witness, release) == Err(SendError::Unsupported) {
                tracing::info!(
                    witness,
                    "the witness's version has no lease release; the standby waits the lease out"
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "node_tests.rs"]
mod node_tests;
