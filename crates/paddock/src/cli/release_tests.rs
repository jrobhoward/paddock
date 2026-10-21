#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn node(id: u32, role: &str, version: &str, previous: Option<&str>) -> View {
    View {
        id,
        role: role.into(),
        may_run: role == "primary",
        partner_outdated: false,
        version: version.into(),
        previous: previous.map(String::from),
        status: if role == "primary" {
            "running"
        } else {
            "standby"
        }
        .into(),
    }
}

#[test]
fn plan____both_at_the_old_version____standby_first_then_primary_then_done() {
    let before = [
        node(1, "primary", "1.0.0", None),
        node(2, "standby", "1.0.0", None),
    ];
    let half = [
        node(1, "primary", "1.0.0", None),
        node(2, "standby", "2.0.0", Some("1.0.0")),
    ];
    let after = [
        node(1, "primary", "2.0.0", Some("1.0.0")),
        node(2, "standby", "2.0.0", Some("1.0.0")),
    ];

    let steps = [
        plan(&before, "2.0.0"),
        plan(&half, "2.0.0"),
        plan(&after, "2.0.0"),
    ];

    assert_eq!(steps, [Step::Change(2), Step::Change(1), Step::Done]);
}

#[test]
fn plan____no_running_primary_or_standby_catching_up____blocked() {
    let mut no_lease = node(1, "primary", "1.0.0", None);
    no_lease.may_run = false;
    let mut outdated = node(1, "primary", "1.0.0", None);
    outdated.partner_outdated = true;
    let standby = node(2, "standby", "1.0.0", None);

    let steps = [
        plan(&[no_lease, standby.clone()], "2.0.0"),
        plan(&[node(1, "down", "1.0.0", None), standby.clone()], "2.0.0"),
        plan(&[outdated, standby], "2.0.0"),
    ];

    assert!(
        matches!(&steps[0], Step::Blocked(w) if w.contains("node 1 is primary")),
        "{steps:?}"
    );
    assert!(matches!(&steps[1], Step::Blocked(w) if w.contains("node 1 is down")));
    assert!(matches!(&steps[2], Step::Blocked(w) if w.contains("node 2 is catching up")));
}

#[test]
fn rollback_target____same_version_or_half_way____the_previous() {
    let both = [
        node(1, "primary", "2.0.0", Some("1.0.0")),
        node(2, "standby", "2.0.0", Some("1.0.0")),
    ];
    let half = [
        node(1, "primary", "2.0.0", Some("1.0.0")),
        node(2, "standby", "1.0.0", Some("2.0.0")),
    ];
    let unrelated = [
        node(1, "primary", "3.0.0", Some("2.0.0")),
        node(2, "standby", "1.0.0", None),
    ];

    assert_eq!(rollback_target(&both), Ok("1.0.0".into()));
    assert_eq!(rollback_target(&half), Ok("1.0.0".into()));
    assert!(rollback_target(&unrelated).is_err());
    assert!(
        rollback_target(&[
            node(1, "primary", "1.0.0", None),
            node(2, "standby", "1.0.0", None)
        ])
        .is_err()
    );
}

#[test]
fn parse____socket_or_address_and_key____accepted_else_refused() {
    let good = "[[node]]\nid = 1\nsocket = \"/run/a.sock\"\n\n[[node]]\nid = 2\naddress = \"10.0.0.2:7443\"\nkey = \"ed25519:abc\"\n";
    let one = "[[node]]\nid = 1\nsocket = \"/run/a.sock\"\n";
    let same = "[[node]]\nid = 1\nsocket = \"a\"\n[[node]]\nid = 1\nsocket = \"b\"\n";
    let no_key = "[[node]]\nid = 1\nsocket = \"a\"\n[[node]]\nid = 2\naddress = \"h:1\"\n";
    let both = "[[node]]\nid = 1\nsocket = \"a\"\naddress = \"h:1\"\nkey = \"k\"\n[[node]]\nid = 2\nsocket = \"b\"\n";

    let parsed = PairFile::parse(good).unwrap();

    assert_eq!(parsed.node[1].address.as_deref(), Some("10.0.0.2:7443"));
    assert!(PairFile::parse(one).is_err());
    assert!(PairFile::parse(same).is_err());
    assert!(PairFile::parse(no_key).unwrap_err().contains("node 2"));
    assert!(PairFile::parse(both).unwrap_err().contains("node 1"));
    assert!(PairFile::parse("[[node]]\nid = 1\nport = 3\n").is_err());
}
