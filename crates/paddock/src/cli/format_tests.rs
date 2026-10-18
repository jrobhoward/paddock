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
        listeners: vec![],
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

#[test]
fn instances____restarted_instance____memory_restarts_and_last_exit() {
    let info = InstanceInfo {
        pid: "0.3.2".into(),
        app: "orders".into(),
        version: "1.0.0".into(),
        state: "active".into(),
        queued: 0,
        dropped: 0,
        memory_bytes: 3 << 19,
        restarts: 1,
        last_exit: Some("killed".into()),
        links: Vec::new(),
        monitoring: Vec::new(),
        monitored_by: Vec::new(),
    };

    let out = instances(&[info]);

    let row = out.lines().nth(1).unwrap();
    assert!(row.contains("1.5MiB"), "{out}");
    assert!(row.trim_end().ends_with("1         killed"), "{out}");
}

#[test]
fn cbor_from_json____round_trip____reply_prints_same_json() {
    let bytes = cbor_from_json(r#"{"id":7,"tags":["a","b"]}"#).unwrap();

    let shown = reply("application/cbor", &bytes);
    let text = reply("text/plain", b"plain");
    let bad = cbor_from_json("not json");

    assert_eq!(shown, r#"{"id":7,"tags":["a","b"]}"#);
    assert_eq!(text, "plain");
    assert!(bad.unwrap_err().starts_with("BODY is not JSON"));
}

#[test]
fn detail____connection_handler____lists_listeners() {
    let d = AppDetail {
        summary: summary("modbus-gw", "running"),
        signer: "RWQ".into(),
        granted: vec![],
        denied: vec![],
        stubbed: vec![],
        warnings: vec![],
        restarts: 0,
        instance_list: vec![],
        listeners: vec![paddock::api::ListenerInfo {
            name: "modbus".into(),
            address: "0.0.0.0:1502".into(),
            active: 3,
            accepted: 40,
            refused: 1,
            crashed: 2,
            idle_closed: 5,
        }],
    };

    let out = detail(&d);

    let row = out.lines().last().unwrap();
    assert!(out.contains("LISTENER"), "{out}");
    assert!(
        row.starts_with("modbus") && row.contains("0.0.0.0:1502"),
        "{out}"
    );
}

fn primary() -> paddock::api::FailoverInfo {
    paddock::api::FailoverInfo {
        partner: 2,
        arbiter: "witness 3".into(),
        role: "primary".into(),
        epoch: 3,
        may_run: true,
        awaiting_confirmation: false,
        partner_reachable: true,
        paths: [("link".to_string(), true)].into(),
        singletons: vec![],
        generation: 7,
        partner_outdated: false,
        replication: None,
    }
}

#[test]
fn failover____primary_with_cluster_buckets____replication_line() {
    let mut info = primary();
    info.partner_outdated = true;
    info.replication = Some(paddock::api::ReplicationInfo {
        buckets: vec!["points".into()],
        commit: 40,
        last: 42,
        base: 0,
        partner_matched: Some(30),
        alone: true,
    });

    let out = failover(&info);

    assert!(out.contains("\noutdated:   the partner lacks"), "{out}");
    assert!(
        out.ends_with(
            "\nreplicated: points (commit 40, last 42, partner at 30, 12 behind, committing alone)"
        ),
        "{out}"
    );
}

#[test]
fn failover_watch____primary____leadership_fields_only() {
    let info = primary();

    let text = failover_watch(Some(&info), false);
    let json = failover_watch(Some(&info), true);

    assert_eq!(text, "role=primary epoch=3 may_run=yes");
    assert_eq!(
        json,
        r#"{"awaiting_confirmation":false,"epoch":3,"may_run":true,"role":"primary"}"#
    );
}

#[test]
fn failover_watch____node_unreachable____role_unreachable_may_not_run() {
    let text = failover_watch(None, false);
    let json = failover_watch(None, true);

    assert_eq!(text, "role=unreachable may_run=no");
    assert_eq!(json, r#"{"may_run":false,"role":"unreachable"}"#);
}

#[test]
fn failover____standby_waiting____says_how_to_confirm() {
    let info = paddock::api::FailoverInfo {
        partner: 1,
        arbiter: "operator".into(),
        role: "standby".into(),
        epoch: 4,
        may_run: false,
        awaiting_confirmation: true,
        partner_reachable: false,
        paths: [("link".to_string(), false), ("serial".to_string(), true)].into(),
        singletons: vec!["leader".into()],
        generation: 2,
        partner_outdated: false,
        replication: None,
    };

    let out = failover(&info);

    assert!(
        out.starts_with("role:       standby\nepoch:      4\nmay run:    no"),
        "{out}"
    );
    assert!(out.contains("partner:    1 (reachable: no)"), "{out}");
    assert!(out.contains("singletons: leader"), "{out}");
    assert!(
        out.contains("paths:      link silent, serial heard"),
        "{out}"
    );
    assert!(out.contains("paddock failover confirm"), "{out}");
}

#[test]
fn event____fields____key_value_with_spaced_strings_quoted() {
    let e: paddock::api::Event = serde_json::from_value(serde_json::json!({
        "time_ms": 5, "kind": "instance", "event": "exit",
        "app": "counter", "pid": "0.1.1@1", "reason": "crashed: oops", "code": 3, "empty": ""
    }))
    .unwrap();

    let out = event(&e);

    assert_eq!(
        out,
        "5 instance exit app=counter code=3 empty=\"\" pid=0.1.1@1 reason=\"crashed: oops\""
    );
}

#[test]
fn members_named____a_contact____contact_column_with_dash_for_others() {
    let member = |node| paddock::api::MemberInfo {
        node,
        creation: 1,
        address: format!("10.0.0.{node}:9000"),
        role: "member".into(),
        reachable: true,
    };
    let names = std::collections::BTreeMap::from([(1, "alpha".to_string())]);

    let named = members_named(&[member(1), member(2)], &names);
    let plain = members(&[member(1)]);

    assert_eq!(
        named,
        "NODE  CREATION  ROLE     REACHABLE  CONTACT     ADDRESS\n\
         1     1         member   yes        alpha       10.0.0.1:9000\n\
         2     1         member   yes        -           10.0.0.2:9000"
    );
    assert_eq!(
        plain,
        "NODE  CREATION  ROLE     REACHABLE  ADDRESS\n1     1         member   yes        10.0.0.1:9000"
    );
}
