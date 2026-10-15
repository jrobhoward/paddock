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
        yielding: false,
    })
}

/// The partner says `status` on the link, heard now.
fn partner_says(env: &Fake, standing: Standing, promoting: bool, yields: bool, epoch: u64) {
    let status = Status {
        standing,
        promoting,
        yields,
        epoch,
        stamp: 2 << 32,
        echo: 0,
    };
    *env.beats.lock().unwrap() = vec![Beat {
        path: crate::transport::liveness::LINK.to_string(),
        live: true,
        heard: Instant::now(),
        payload: status.encode(),
    }];
}

/// Node 1 as primary at epoch 1 with a lease: resumed with the witness's
/// grant.
async fn primary(env: &Fake) -> Node {
    let mut ctx = ctx();
    wait_out().await;
    let state = down(1).step(&mut ctx, env, Instant::now());
    let state = state.on_message(
        &mut ctx,
        FailoverMsg::Granted { seq: 0, epoch: 1 },
        Instant::now(),
    );
    Node { state, ctx }
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

#[tokio::test(start_paused = true)]
async fn switchover____primary_with_a_confirming_standby____steps_down_yielding_and_resumes_late() {
    let env = Fake::default();
    let mut node = primary(&env).await;
    partner_says(&env, Standing::Standby, false, false, 1);
    let switched = Instant::now();

    let result = node.switchover(&env, switched, false);
    let leadership = node.state.leadership(&node.ctx.cfg, switched);
    let status = node.state.status(&node.ctx);
    // Silent partner from now on: no rejoin, so only a resume can follow.
    env.beats.lock().unwrap().clear();
    let asked_before = env.requests().len();
    tokio::time::advance(Duration::from_millis(3_600)).await;
    node.apply(|state, ctx| state.step(ctx, &env, Instant::now()));
    let after_a_down_nodes_wait = env.requests().len();
    tokio::time::advance(Duration::from_millis(3_000)).await;
    node.apply(|state, ctx| state.step(ctx, &env, Instant::now()));

    assert_eq!(result, Ok(()));
    assert!(!leadership.may_run);
    assert_eq!(leadership.standing, Standing::Down);
    assert!(status.yields && status.standing == Standing::Down && status.epoch == 1);
    assert_eq!(
        after_a_down_nodes_wait, asked_before,
        "no resume within lease + margin"
    );
    assert_eq!(
        env.requests().last(),
        Some(&(1, true)),
        "resumes a lease later"
    );
}

#[tokio::test(start_paused = true)]
async fn switchover____not_a_primary_or_no_confirming_standby____refused() {
    let env = Fake::default();
    let mut not_primary = Node {
        state: down(1),
        ctx: ctx(),
    };
    let mut alone = primary(&env).await;
    let mut asking = primary(&env).await;

    let refused_down = not_primary.switchover(&env, Instant::now(), false);
    let refused_silent = alone.switchover(&env, Instant::now(), false);
    partner_says(&env, Standing::Standby, true, false, 1);
    let refused_promoting = asking.switchover(&env, Instant::now(), false);

    assert_eq!(refused_down, Err(SwitchoverRefused::NotPrimary));
    assert_eq!(refused_silent, Err(SwitchoverRefused::NoStandby));
    assert_eq!(refused_promoting, Err(SwitchoverRefused::NoStandby));
    assert!(matches!(alone.state, State::Primary(_)));
}

#[tokio::test(start_paused = true)]
async fn standby____partner_yields____asks_at_once_and_does_not_stand_down() {
    let env = Fake::default();
    let mut ctx = ctx();
    partner_says(&env, Standing::Primary, false, false, 1);
    let standby = down(1).step(&mut ctx, &env, Instant::now());
    let rejoined = matches!(standby, State::Standby(_));

    partner_says(&env, Standing::Down, false, true, 1);
    let state = standby.step(&mut ctx, &env, Instant::now());
    let asked = env.requests();
    tokio::time::advance(Duration::from_millis(3_600)).await;
    partner_says(&env, Standing::Down, false, true, 1);
    // Past lease + margin hearing `Down`: a standby would stand down, but
    // not for a yield; it asks again instead.
    let state = state.step(&mut ctx, &env, Instant::now());
    let still_standby = matches!(state, State::Standby(_));
    let state = state.on_message(
        &mut ctx,
        FailoverMsg::Granted { seq: 1, epoch: 2 },
        Instant::now(),
    );

    assert!(rejoined);
    assert!(still_standby);
    assert_eq!(
        asked,
        vec![(0, false)],
        "asked on the first step that heard the yield"
    );
    assert!(matches!(state, State::Primary(ref p) if p.epoch == 2));
}
