#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::LifecycleState::{self, *};

#[rstest]
#[case(Installed, Starting)]
#[case(Starting, Active)]
#[case(Starting, Stopping)]
#[case(Starting, Failed)]
#[case(Active, Stopping)]
#[case(Active, Upgrading)]
#[case(Active, Failed)]
#[case(Upgrading, Active)]
#[case(Upgrading, Failed)]
#[case(Stopping, Stopped)]
#[case(Stopping, Failed)]
#[case(Stopped, Starting)]
#[case(Stopped, Restarting)]
#[case(Failed, Restarting)]
#[case(Restarting, Starting)]
#[case(Restarting, Failed)]
fn can_transition_to____allowed____returns_true(
    #[case] from: LifecycleState,
    #[case] to: LifecycleState,
) {
    assert!(from.can_transition_to(to));
}

#[rstest]
#[case(Installed, Active)]
#[case(Starting, Stopped)]
#[case(Active, Starting)]
#[case(Active, Stopped)]
#[case(Stopped, Active)]
#[case(Failed, Active)]
#[case(Failed, Starting)]
#[case(Stopped, Failed)]
#[case(Failed, Failed)]
#[case(Upgrading, Stopping)]
fn can_transition_to____not_allowed____returns_false(
    #[case] from: LifecycleState,
    #[case] to: LifecycleState,
) {
    assert!(!from.can_transition_to(to));
}

#[test]
fn handles_messages____only_active____returns_true() {
    let handling: Vec<LifecycleState> = [
        Installed, Starting, Active, Upgrading, Stopping, Stopped, Failed, Restarting,
    ]
    .into_iter()
    .filter(|s| s.handles_messages())
    .collect();

    assert_eq!(handling, vec![Active]);
}
