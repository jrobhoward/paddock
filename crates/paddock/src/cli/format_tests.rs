#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn summary(name: &str, status: &str) -> AppSummary {
    AppSummary {
        name: name.into(),
        version: "1.4.0".into(),
        status: status.into(),
        reason: None,
        instances: 2,
        previous: None,
    }
}

#[test]
fn table____cells_of_different_widths____columns_aligned() {
    let out = table(
        &["A", "LONG HEADER"],
        &[vec!["wide cell".into(), "x".into()]],
    );

    assert_eq!(out, "A          LONG HEADER\nwide cell  x");
}

#[test]
fn apps____two_apps____one_row_each_under_header() {
    let out = apps(&[summary("orders", "running"), summary("billing", "failed")]);

    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].starts_with("APP"));
    assert!(lines[1].starts_with("orders   1.4.0    running  2"));
}

#[test]
fn apps____none____says_so() {
    assert_eq!(apps(&[]), "no apps deployed");
}

#[test]
fn detail____failed_app____shows_reason_and_denied() {
    let mut s = summary("orders", "failed");
    s.reason = Some("crashed: max restart intensity".into());
    let d = AppDetail {
        summary: s,
        signer: "RWQ".into(),
        granted: vec!["wasi:logging".into()],
        denied: vec!["wasi:keyvalue".into()],
        stubbed: vec![],
        warnings: vec!["messaging_acl is not enforced until Phase 2".into()],
        restarts: 3,
        instance_list: vec![],
    };

    let out = detail(&d);

    assert!(out.contains("reason:    crashed: max restart intensity"));
    assert!(out.contains("denied:    wasi:keyvalue"));
    assert!(out.contains("warning:   messaging_acl"));
}

#[test]
fn upgrade____snapshot____reports_longest_pause() {
    let u = UpgradeResponse {
        from: "1.0.0".into(),
        to: "2.0.0".into(),
        mode: "snapshot".into(),
        instances: vec!["0.1.1".into(), "0.2.1".into()],
        pauses_us: vec![300, 900],
        approved: vec![],
    };

    assert_eq!(
        upgrade(&u),
        "1.0.0 -> 2.0.0 (snapshot), 2 instance(s), longest pause 900 µs"
    );
}
