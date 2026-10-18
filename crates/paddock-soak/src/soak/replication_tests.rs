#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(ToString::to_string).collect()
}

fn ledger(acked: &[&str], unknown: &[&str], refused: &[&str]) -> Ledger {
    Ledger {
        acked: set(acked),
        unknown: set(unknown),
        refused: set(refused),
        next: 0,
    }
}

#[test]
fn outcome____replies____sorted() {
    let cases = [
        (Some("ok"), Outcome::Acked),
        (
            Some("err:storage: unavailable: no primary with a lease can take it now"),
            Outcome::Refused,
        ),
        (
            Some(
                "err:storage: unavailable: not committed in time, or the primary stepped down; the write may yet commit",
            ),
            Outcome::Unknown,
        ),
        (None, Outcome::Unknown),
    ];

    for (reply, expected) in cases {
        assert_eq!(outcome(reply), expected, "{reply:?}");
    }
}

#[test]
fn problems____copies_agree_and_hold_every_acknowledged_write____none() {
    let l = ledger(&["w1", "w2"], &["w3"], &["w4"]);
    let keys = set(&["w1", "w2", "w3"]);

    assert!(problems(&l, [&keys, &keys]).is_empty());
}

#[test]
fn problems____lost_differing_and_refused_writes____each_reported() {
    let l = ledger(&["w1", "w2"], &[], &["w4"]);
    let one = set(&["w1", "w4"]);
    let two = set(&["w1"]);

    let found = problems(&l, [&one, &two]);

    assert!(
        found.iter().any(|p| p.contains("copies differ")),
        "{found:?}"
    );
    assert!(
        found.iter().any(|p| p.contains("node 1 lacks")),
        "{found:?}"
    );
    assert!(
        found.iter().any(|p| p.contains("node 2 lacks")),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|p| p.contains("node 1 holds writes refused")),
        "{found:?}"
    );
}
