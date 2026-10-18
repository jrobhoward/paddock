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
        seq: 5,
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
        Decision::Record(record) => Some(witness.apply_record(record)),
    }
}

fn config(resume: Option<(u64, u32)>) -> WitnessConfig {
    let mut cfg = WitnessConfig::new([1, 2], 1);
    cfg.resume = resume.map(|(epoch, promoted)| WitnessState::new(epoch, promoted, false));
    cfg
}

#[tokio::test(start_paused = true)]
async fn Witness____fresh_and_partner_long_unconfirmed____grants() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));

    let answer = request(&mut witness);

    assert_eq!(
        answer,
        Some(FailoverMsg::Granted {
            seq: 5,
            epoch: 2,
            outdated: false
        })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____resumed____waits_a_lease_from_the_restart() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(Some((1, 1))));

    let at_restart = request(&mut witness);
    tokio::time::advance(Duration::from_millis(3_501)).await;
    let after_hold = request(&mut witness);

    assert_eq!(at_restart, Some(FailoverMsg::NotYet { wait_ms: 3_501 }));
    assert_eq!(
        after_hold,
        Some(FailoverMsg::Granted {
            seq: 5,
            epoch: 2,
            outdated: false
        })
    );
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
            seq: 5,
            epoch: 2,
            promoted: 2,
            outdated: false,
        }
    );
    assert_eq!(state_while_keeping, (1, 1));
    assert_eq!(renewal, None);
    assert_eq!(second, None);
    assert_eq!(
        granted,
        FailoverMsg::Granted {
            seq: 5,
            epoch: 2,
            outdated: false
        }
    );
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
    assert_eq!(
        again,
        Some(FailoverMsg::Granted {
            seq: 5,
            epoch: 2,
            outdated: false
        })
    );
}

fn record_msg(seq: u64, epoch: u64, outdated: bool) -> FailoverMsg {
    FailoverMsg::Record {
        seq,
        epoch,
        outdated,
    }
}

#[tokio::test(start_paused = true)]
async fn Witness____partner_recorded_out_of_date____not_promoted_until_cleared() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));

    let set = witness.handle(1, record_msg(3, 1, true));
    let Some(Decision::Record(record)) = set else {
        panic!("{set:?}");
    };
    let kept = record.state;
    let recorded = witness.apply_record(record);
    let while_outdated = request(&mut witness);
    let cleared = answer(&mut witness, 1, record_msg(4, 1, false));
    let after_clearing = request(&mut witness);

    assert_eq!(kept, WitnessState::new(1, 1, true));
    assert_eq!(
        recorded,
        FailoverMsg::Recorded {
            seq: 3,
            outdated: true
        }
    );
    assert_eq!(while_outdated, Some(FailoverMsg::Refused { epoch: 1 }));
    assert_eq!(
        cleared,
        Some(FailoverMsg::Recorded {
            seq: 4,
            outdated: false
        })
    );
    assert_eq!(
        after_clearing,
        Some(FailoverMsg::Granted {
            seq: 5,
            epoch: 2,
            outdated: false
        })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____record_from_the_partner_or_a_stale_epoch____fenced_and_unchanged() {
    let mut witness = Witness::start(config(Some((2, 1))));

    let from_partner = answer(&mut witness, 2, record_msg(1, 2, true));
    let stale = answer(&mut witness, 1, record_msg(2, 1, true));

    assert_eq!(from_partner, Some(FailoverMsg::Fenced { epoch: 2 }));
    assert_eq!(stale, Some(FailoverMsg::Fenced { epoch: 2 }));
    assert!(!witness.outdated);
}

#[tokio::test(start_paused = true)]
async fn Witness____record_already_as_asked____answered_without_keeping() {
    let mut witness = Witness::start(config(None));

    let decision = witness.handle(1, record_msg(6, 1, false));

    assert_eq!(
        decision,
        Some(Decision::Answer(FailoverMsg::Recorded {
            seq: 6,
            outdated: false
        }))
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____resumed_with_the_record____resume_keeps_it_and_the_partner_is_refused() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut cfg = WitnessConfig::new([1, 2], 1);
    cfg.resume = Some(WitnessState::new(2, 1, true));
    let mut witness = Witness::start(cfg);
    tokio::time::advance(Duration::from_secs(4)).await;

    let resumed = answer(
        &mut witness,
        1,
        FailoverMsg::Request {
            seq: 9,
            resume: true,
            epoch: 2,
            confirmed_ago_ms: 5_000,
        },
    );
    let partner = answer(
        &mut witness,
        2,
        FailoverMsg::Request {
            seq: 1,
            resume: false,
            epoch: 2,
            confirmed_ago_ms: 5_000,
        },
    );

    assert_eq!(
        resumed,
        Some(FailoverMsg::Granted {
            seq: 9,
            epoch: 2,
            outdated: true
        })
    );
    assert_eq!(partner, Some(FailoverMsg::Refused { epoch: 2 }));
}
