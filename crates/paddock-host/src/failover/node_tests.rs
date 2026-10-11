#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

//! Transitions driven one at a time against a fake [`Env`], on a paused
//! clock: the cases the model checks that the simulation reaches only by
//! chance.

use std::sync::Mutex;

use super::*;

const PARTNER: u32 = 2;
const WITNESS: u32 = 3;

/// A node 1 whose partner and witness are silent unless a test says
/// otherwise; it records what it sends.
#[derive(Default)]
struct Fake {
    beats: Mutex<Vec<Beat>>,
    sent: Mutex<Vec<(u32, FailoverMsg)>>,
}

impl Env for Fake {
    fn node(&self) -> u32 {
        1
    }

    fn beats_from(&self, node: u32) -> Vec<Beat> {
        if node == PARTNER {
            self.beats.lock().unwrap().clone()
        } else {
            Vec::new()
        }
    }

    fn reachable(&self, _node: u32) -> bool {
        true
    }

    fn send(&self, to: u32, msg: FailoverMsg) {
        self.sent.lock().unwrap().push((to, msg));
    }
}

impl Fake {
    /// The requests sent to the witness: (seq, resume).
    fn requests(&self) -> Vec<(u64, bool)> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(to, m)| match m {
                FailoverMsg::Request { seq, resume, .. } if *to == WITNESS => Some((*seq, *resume)),
                _ => None,
            })
            .collect()
    }
}

fn ctx() -> Ctx {
    let cfg = FailoverConfig::new(PARTNER, Arbiter::Witness(WITNESS));
    let initial = Leadership {
        standing: Standing::Down,
        epoch: 0,
        may_run: false,
        awaiting_confirmation: false,
    };
    Ctx {
        rt: Weak::new(),
        cfg,
        confirmed_at: Instant::now(),
        confirming: false,
        stamp: 1 << 32,
        stamps: BTreeMap::new(),
        seen: 0,
        requests: BTreeMap::new(),
        next_request: 0,
        confirmed_by_operator: false,
        late_logged: None,
        extended_logged: None,
        tx: watch::channel(initial).0,
    }
}

fn down(epoch: u64) -> State {
    State::Down(Down {
        epoch,
        asking: Asking::default(),
    })
}

/// Wait out a lease and its margin, so a node that is down may ask to
/// resume.
async fn wait_out() {
    tokio::time::advance(Duration::from_millis(3_600)).await;
}

#[tokio::test(start_paused = true)]
async fn down____not_yet____waits_the_time_given_then_asks_again_and_resumes() {
    let env = Fake::default();
    let mut ctx = ctx();
    wait_out().await;
    let state = down(1).step(&mut ctx, &env, Instant::now());
    let asked = env.requests();

    let state = state.on_message(&mut ctx, FailoverMsg::NotYet { wait_ms: 5 }, Instant::now());
    let state = state.step(&mut ctx, &env, Instant::now());
    let before_the_wait = env.requests();
    tokio::time::advance(Duration::from_millis(6)).await;
    let state = state.step(&mut ctx, &env, Instant::now());
    let after_the_wait = env.requests();
    let state = state.on_message(
        &mut ctx,
        FailoverMsg::Granted { seq: 1, epoch: 1 },
        Instant::now(),
    );

    assert_eq!(asked, vec![(0, true)]);
    assert_eq!(before_the_wait, asked);
    assert_eq!(after_the_wait, vec![(0, true), (1, true)]);
    assert!(matches!(state, State::Primary(ref p) if p.epoch == 1 && p.lease_until.is_some()));
}

#[tokio::test(start_paused = true)]
async fn down____grant_for_a_request_it_never_sent____ignored() {
    let env = Fake::default();
    let mut ctx = ctx();
    wait_out().await;
    let state = down(1).step(&mut ctx, &env, Instant::now());

    let state = state.on_message(
        &mut ctx,
        FailoverMsg::Granted { seq: 7, epoch: 1 },
        Instant::now(),
    );

    assert!(matches!(state, State::Down(_)));
}

#[tokio::test(start_paused = true)]
async fn down____refused____takes_the_witness_epoch_and_waits_a_lease() {
    let env = Fake::default();
    let mut ctx = ctx();
    wait_out().await;
    let state = down(1).step(&mut ctx, &env, Instant::now());

    let state = state.on_message(&mut ctx, FailoverMsg::Refused { epoch: 4 }, Instant::now());
    tokio::time::advance(Duration::from_millis(2_900)).await;
    let state = state.step(&mut ctx, &env, Instant::now());
    let within_a_lease = env.requests().len();
    tokio::time::advance(Duration::from_millis(200)).await;
    let state = state.step(&mut ctx, &env, Instant::now());

    assert_eq!(state.epoch(), 4);
    assert_eq!(within_a_lease, 1);
    assert_eq!(env.requests().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn primary____fenced____steps_down_only_for_a_higher_epoch() {
    let env = Fake::default();
    let mut ctx = ctx();
    wait_out().await;
    let state = down(1).step(&mut ctx, &env, Instant::now());
    let primary = state.on_message(
        &mut ctx,
        FailoverMsg::Granted { seq: 0, epoch: 2 },
        Instant::now(),
    );

    let kept = primary.on_message(&mut ctx, FailoverMsg::Fenced { epoch: 2 }, Instant::now());
    let still_primary = matches!(kept, State::Primary(_));
    let fenced = kept.on_message(&mut ctx, FailoverMsg::Fenced { epoch: 3 }, Instant::now());

    assert!(still_primary);
    assert!(matches!(fenced, State::Down(ref d) if d.epoch == 3));
}

#[tokio::test(start_paused = true)]
async fn primary____lease_ok_for_a_renewal____extends_from_its_send_time() {
    let env = Fake::default();
    let mut ctx = ctx();
    wait_out().await;
    let state = down(1).step(&mut ctx, &env, Instant::now());
    let state = state.on_message(
        &mut ctx,
        FailoverMsg::Granted { seq: 0, epoch: 1 },
        Instant::now(),
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    let renewed_at = Instant::now();
    let state = state.step(&mut ctx, &env, renewed_at);
    tokio::time::advance(Duration::from_millis(400)).await;

    let state = state.on_message(&mut ctx, FailoverMsg::LeaseOk { seq: 0 }, Instant::now());

    let expected = renewed_at + ctx.cfg.lease - ctx.cfg.margin;
    assert_eq!(state.lease_end(), Some(expected));
}
