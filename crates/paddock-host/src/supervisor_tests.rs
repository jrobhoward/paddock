#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use rstest::rstest;

use super::*;

#[rstest]
#[case(RestartType::Permanent, ExitReason::Normal, true)]
#[case(RestartType::Permanent, ExitReason::Crashed("x".into()), true)]
#[case(RestartType::Transient, ExitReason::Normal, false)]
#[case(RestartType::Transient, ExitReason::Shutdown, false)]
#[case(RestartType::Transient, ExitReason::Killed, true)]
#[case(RestartType::Transient, ExitReason::Timeout, true)]
#[case(RestartType::Temporary, ExitReason::Crashed("x".into()), false)]
fn should_restart____restart_type_and_reason____matches_otp(
    #[case] restart: RestartType,
    #[case] reason: ExitReason,
    #[case] expected: bool,
) {
    assert_eq!(should_restart(restart, &reason), expected);
}

fn backoff(jitter: bool) -> Backoff {
    Backoff {
        initial: Duration::from_millis(100),
        max: Duration::from_millis(1000),
        jitter,
    }
}

#[rstest]
#[case(1, 100)]
#[case(2, 200)]
#[case(3, 400)]
#[case(4, 800)]
#[case(5, 1000)]
#[case(40, 1000)]
fn Backoff____without_jitter____doubles_up_to_max(#[case] n: usize, #[case] ms: u64) {
    let delay = backoff(false).delay(n, 0.7);

    assert_eq!(delay, Duration::from_millis(ms));
}

#[rstest]
#[case(0.0, 200)]
#[case(0.5, 300)]
#[case(0.999_999, 400)]
fn Backoff____with_jitter____between_half_and_full(#[case] unit: f64, #[case] ms: u64) {
    let delay = backoff(true).delay(3, unit);

    let diff = delay.as_secs_f64() - Duration::from_millis(ms).as_secs_f64();
    assert!(diff.abs() < 0.001, "{delay:?}");
}

#[test]
fn Backoff____none____always_zero() {
    let delay = Backoff::none().delay(5, 0.5);

    assert_eq!(delay, Duration::ZERO);
}
