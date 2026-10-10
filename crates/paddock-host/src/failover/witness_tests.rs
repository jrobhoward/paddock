#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

/// A request from node 2 whose last confirmation of node 1's lease was 5 s
/// ago, longer than the 3.5 s hold.
fn request(witness: &mut Witness) -> Option<FailoverMsg> {
    answer(witness, 2, request_msg())
}

fn request_msg() -> FailoverMsg {
    FailoverMsg::Request {
        resume: false,
        epoch: 1,
        confirmed_ago_ms: 5_000,
    }
}

/// The witness's answer to `msg` from `from`, applying a grant at once (as
/// with nothing to keep on disk).
fn answer(witness: &mut Witness, from: u32, msg: FailoverMsg) -> Option<FailoverMsg> {
    match witness.handle(from, msg)? {
        Decision::Answer(reply) => Some(reply),
        Decision::Grant(grant) => Some(witness.apply(grant)),
    }
}

fn config(resume: Option<(u64, u32)>) -> WitnessConfig {
    let mut cfg = WitnessConfig::new([1, 2], 1);
    cfg.resume = resume;
    cfg
}

#[tokio::test(start_paused = true)]
async fn Witness____fresh_and_partner_long_unconfirmed____grants() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));

    let answer = request(&mut witness);

    assert_eq!(answer, Some(FailoverMsg::Granted { epoch: 2 }));
}

#[tokio::test(start_paused = true)]
async fn Witness____resumed____waits_a_lease_from_the_restart() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(Some((1, 1))));

    let at_restart = request(&mut witness);
    tokio::time::advance(Duration::from_millis(3_501)).await;
    let after_hold = request(&mut witness);

    assert_eq!(at_restart, Some(FailoverMsg::NotYet { wait_ms: 3_501 }));
    assert_eq!(after_hold, Some(FailoverMsg::Granted { epoch: 2 }));
}

#[tokio::test(start_paused = true)]
async fn Witness____grant_decided_not_yet_kept____state_unchanged_and_the_pair_unanswered() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));

    let decided = witness.handle(2, request_msg());
    let state_while_keeping = (witness.epoch, witness.promoted);
    let renewal = witness.handle(1, FailoverMsg::Renew { seq: 7, epoch: 1 });
    let second = witness.handle(2, request_msg());
    let Some(Decision::Grant(grant)) = decided else {
        panic!("{decided:?}");
    };
    let granted = witness.apply(grant);
    let old_primary_after = answer(&mut witness, 1, FailoverMsg::Renew { seq: 8, epoch: 1 });

    assert_eq!(
        grant,
        Grant {
            to: 2,
            epoch: 2,
            promoted: 2
        }
    );
    assert_eq!(state_while_keeping, (1, 1));
    assert_eq!(renewal, None);
    assert_eq!(second, None);
    assert_eq!(granted, FailoverMsg::Granted { epoch: 2 });
    assert_eq!((witness.epoch, witness.promoted), (2, 2));
    assert_eq!(old_primary_after, Some(FailoverMsg::Fenced { epoch: 2 }));
}

#[tokio::test(start_paused = true)]
async fn Witness____grant_not_kept____abandoned_and_asked_again() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));

    let first = witness.handle(2, request_msg());
    witness.abandon();
    let again = request(&mut witness);

    assert!(matches!(first, Some(Decision::Grant(_))), "{first:?}");
    assert_eq!(again, Some(FailoverMsg::Granted { epoch: 2 }));
}
