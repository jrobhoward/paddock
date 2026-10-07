#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use super::*;

#[test]
fn admit____no_limit____always_emits() {
    let limits = LogLimits::default();

    let all = (0..1000).all(|_| limits.admit("a", None) == Admit::Emit { dropped: 0 });

    assert!(all);
}

#[test]
fn admit____burst_past_rate____excess_dropped_then_counted() {
    let limits = LogLimits::default();
    let t0 = Instant::now();

    let burst: Vec<Admit> = (0..5).map(|_| limits.admit_at("a", Some(3), t0)).collect();
    let later = limits.admit_at("a", Some(3), t0 + Duration::from_secs(1));

    assert_eq!(
        burst,
        vec![
            Admit::Emit { dropped: 0 },
            Admit::Emit { dropped: 0 },
            Admit::Emit { dropped: 0 },
            Admit::Drop,
            Admit::Drop
        ]
    );
    assert_eq!(later, Admit::Emit { dropped: 2 });
}

#[test]
fn admit____two_apps____separate_allowances() {
    let limits = LogLimits::default();
    let t0 = Instant::now();
    limits.admit_at("a", Some(1), t0);

    let other = limits.admit_at("b", Some(1), t0);
    let same = limits.admit_at("a", Some(1), t0);

    assert_eq!(other, Admit::Emit { dropped: 0 });
    assert_eq!(same, Admit::Drop);
}
