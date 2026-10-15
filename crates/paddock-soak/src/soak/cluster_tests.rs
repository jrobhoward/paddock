#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use paddock::api::MonitorInfo;

use super::*;

fn inst(
    pid: &str,
    links: &[&str],
    monitoring: &[(u64, &str)],
    monitored_by: &[(u64, &str)],
) -> InstanceInfo {
    let monitors = |v: &[(u64, &str)]| {
        v.iter()
            .map(|(reference, pid)| MonitorInfo {
                reference: *reference,
                pid: (*pid).to_string(),
            })
            .collect()
    };
    InstanceInfo {
        pid: pid.into(),
        app: APP.into(),
        version: "1.0.0".into(),
        state: "active".into(),
        queued: 0,
        dropped: 0,
        memory_bytes: 0,
        restarts: 0,
        last_exit: None,
        links: links.iter().map(|p| (*p).to_string()).collect(),
        monitoring: monitors(monitoring),
        monitored_by: monitors(monitored_by),
    }
}

fn cluster(nodes: Vec<(u32, Vec<InstanceInfo>)>) -> BTreeMap<u32, Vec<InstanceInfo>> {
    nodes.into_iter().collect()
}

#[test]
fn disagreements____both_sides_listed____none() {
    let ps = cluster(vec![
        (
            1,
            vec![inst("1.1.1@1", &["2.1.1@1"], &[(7, "2.1.1@1")], &[])],
        ),
        (
            2,
            vec![inst("2.1.1@1", &["1.1.1@1"], &[], &[(7, "1.1.1@1")])],
        ),
    ]);

    assert!(disagreements(&ps).is_empty());
}

#[test]
fn disagreements____link_on_one_side_only____reported() {
    let ps = cluster(vec![
        (1, vec![inst("1.1.1@1", &["2.1.1@1"], &[], &[])]),
        (2, vec![inst("2.1.1@1", &[], &[], &[])]),
    ]);

    let found = disagreements(&ps);

    assert_eq!(
        found,
        vec!["1.1.1@1 is linked to 2.1.1@1, which does not list it"]
    );
}

#[test]
fn disagreements____entries_naming_gone_instances____reported() {
    let ps = cluster(vec![(
        1,
        vec![inst(
            "1.1.1@1",
            &["2.1.1@1"],
            &[(7, "3.1.1@1")],
            &[(8, "2.1.2@1")],
        )],
    )]);

    let found = disagreements(&ps);

    assert_eq!(found.len(), 3, "{found:?}");
    assert!(
        found.iter().all(|p| p.ends_with("which is gone")),
        "{found:?}"
    );
}

#[test]
fn disagreements____monitor_with_another_reference____reported_both_ways() {
    let ps = cluster(vec![
        (1, vec![inst("1.1.1@1", &[], &[(7, "2.1.1@1")], &[])]),
        (2, vec![inst("2.1.1@1", &[], &[], &[(9, "1.1.1@1")])]),
    ]);

    let found = disagreements(&ps);

    assert_eq!(found.len(), 2, "{found:?}");
}
