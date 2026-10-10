#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

/// A request from node 2 whose last confirmation of node 1's lease was 5 s
/// ago, longer than the 3.5 s hold.
fn request(witness: &mut Witness) -> Option<FailoverMsg> {
    witness.handle(
        2,
        FailoverMsg::Request {
            resume: false,
            epoch: 1,
            confirmed_ago_ms: 5_000,
        },
    )
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
