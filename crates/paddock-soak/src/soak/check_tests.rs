#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn rec(at_us: i64, node: u32, mark: Mark) -> Record {
    Record { at_us, node, mark }
}

#[test]
fn log_records____gate_lines____start_and_stop() {
    let text = "\
2026-10-10T12:00:00.000001Z  INFO paddock::node::gate: singleton starting: this node is primary app=counter
2026-10-10T12:00:00.000002Z  INFO paddock::node::gate: singleton started: this node is primary app=counter
2026-10-10T12:00:01.000000Z  INFO paddock::node::gate: singleton killed: this node may not run it app=counter
2026-10-10T12:00:02.000000Z ERROR paddock::node::gate: cannot start singleton app=counter error=boom
2026-10-10T12:00:03.000000Z  INFO paddock::node::gate: singleton starting: this node is primary app=counter2
not a log line app=counter singleton starting";

    let records = log_records(2, "counter", text);

    let base = parse_rfc3339_us("2026-10-10T12:00:00Z").unwrap();
    assert_eq!(
        records,
        vec![
            rec(base + 1, 2, Mark::Start),
            rec(base + 1_000_000, 2, Mark::Stop),
            rec(base + 2_000_000, 2, Mark::Stop),
        ]
    );
}

#[test]
fn log_records____late_lease_end_and_cut_off_callback____dated_marks() {
    let text = "\
2026-10-10T12:00:09.050000Z  WARN paddock_host::failover::node: failover lease ended before this node noticed; singleton instances stopped running guest code at its end ended_ms_ago=6050
2026-10-10T12:00:09.051000Z  WARN instance{app=counter version=1.0.0 pid=1.4294967297.1 node=1}: paddock_host::instance: instance failed reason=crashed: lease_ended
2026-10-10T12:00:09.052000Z  WARN instance{app=counter2 version=1.0.0 pid=1.4294967298.1 node=1}: paddock_host::instance: instance failed reason=crashed: lease_ended
2026-10-10T12:00:09.053000Z  WARN paddock_host::failover::node: failover lease ended before this node noticed ended_ms_ago=junk";

    let records = log_records(1, "counter", text);

    let base = parse_rfc3339_us("2026-10-10T12:00:00Z").unwrap();
    assert_eq!(
        records,
        vec![
            rec(base + 3_000_000, 1, Mark::LeaseEnded),
            rec(base + 9_051_000, 1, Mark::CutOff),
        ]
    );
}

#[test]
fn log_records____lease_extended____bound_at_until_plus_slack() {
    let text = "\
2026-10-10T12:00:01.000000Z DEBUG paddock::lease: failover lease extended until_ms=2400 slack_ms=500
2026-10-10T12:00:02.000000Z DEBUG paddock::lease: failover lease extended until_ms=2400";

    let records = log_records(1, "counter", text);

    let base = parse_rfc3339_us("2026-10-10T12:00:00Z").unwrap();
    assert_eq!(
        records,
        vec![rec(base + 1_000_000, 1, Mark::LeaseBound(base + 3_900_000))]
    );
}

#[test]
fn log_records____ansi_colours____read() {
    let text = "\u{1b}[2m2026-10-10T12:00:00.000001Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \u{1b}[2mpaddock::node::gate\u{1b}[0m\u{1b}[2m:\u{1b}[0m singleton starting: this node is primary \u{1b}[3mapp\u{1b}[0m\u{1b}[2m=\u{1b}[0mcounter";

    let records = log_records(1, "counter", text);

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].mark, Mark::Start);
}

#[test]
fn running____start_then_process_killed____ends_at_the_kill() {
    let records = [
        rec(10, 1, Mark::Spawn),
        rec(20, 1, Mark::Start),
        rec(50, 1, Mark::Gone),
    ];

    let spans = running(&records, 1, 100);

    assert_eq!(spans, vec![(20, 50)]);
}

#[test]
fn running____still_running_at_the_end____runs_to_the_horizon() {
    let records = [rec(20, 1, Mark::Start), rec(30, 2, Mark::Start)];

    let spans = running(&records, 1, 100);

    assert_eq!(spans, vec![(20, 100)]);
}

#[test]
fn running____frozen_then_thawed____frozen_time_removed() {
    let records = [
        rec(20, 1, Mark::Start),
        rec(30, 1, Mark::Freeze),
        rec(60, 1, Mark::Thaw),
        rec(65, 1, Mark::Stop),
    ];

    let spans = running(&records, 1, 100);

    assert_eq!(spans, vec![(20, 30), (60, 65)]);
}

#[test]
fn running____frozen_then_killed____nothing_after_the_freeze() {
    let records = [
        rec(20, 1, Mark::Start),
        rec(30, 1, Mark::Freeze),
        rec(60, 1, Mark::Gone),
    ];

    let spans = running(&records, 1, 100);

    assert_eq!(spans, vec![(20, 30)]);
}

#[test]
fn running____restarted_without_a_kill_record____ends_at_the_spawn() {
    let records = [
        rec(20, 1, Mark::Start),
        rec(40, 1, Mark::Spawn),
        rec(50, 1, Mark::Start),
    ];

    let spans = running(&records, 1, 70);

    assert_eq!(spans, vec![(20, 40), (50, 70)]);
}

#[test]
fn subtract____hole_across_several_spans____cuts_each() {
    let spans = [(0, 10), (20, 30)];

    let left = subtract(&spans, &[(5, 25)]);

    assert_eq!(left, vec![(0, 5), (25, 30)]);
}

#[test]
fn overlaps____touching_spans____none() {
    let found = overlaps(&[(0, 10)], &[(10, 20)]);

    assert!(found.is_empty());
}

#[test]
fn overlaps____crossing_spans____the_shared_part() {
    let found = overlaps(&[(0, 10), (30, 40)], &[(5, 35)]);

    assert_eq!(found, vec![(5, 10), (30, 35)]);
}

#[test]
fn check_dir____handover_after_a_kill____passes() {
    let dir = tempdir();
    let base = parse_rfc3339_us("2026-10-10T12:00:00Z").unwrap();
    write_events(
        dir.path(),
        &[
            (base, 1, What::Spawn),
            (base, 2, What::Spawn),
            (base + 5_000_000, 1, What::Kill),
            (base + 20_000_000, 0, What::Settled { runners: vec![2] }),
        ],
    );
    write_log(
        dir.path(),
        1,
        &[("2026-10-10T12:00:01Z", "singleton starting")],
    );
    write_log(
        dir.path(),
        2,
        &[("2026-10-10T12:00:09Z", "singleton starting")],
    );

    let report = check_dir(dir.path(), "counter").unwrap();

    assert!(report.passed(), "{}", report.render());
}

#[test]
fn check_dir____old_primary_thawed_after_promotion____overlap_reported() {
    let dir = tempdir();
    let base = parse_rfc3339_us("2026-10-10T12:00:00Z").unwrap();
    write_events(
        dir.path(),
        &[
            (base, 1, What::Spawn),
            (base, 2, What::Spawn),
            (base + 2_000_000, 1, What::Freeze),
            (base + 9_000_000, 1, What::Thaw),
        ],
    );
    write_log(
        dir.path(),
        1,
        &[
            ("2026-10-10T12:00:01Z", "singleton starting"),
            ("2026-10-10T12:00:09.050Z", "singleton killed"),
        ],
    );
    write_log(
        dir.path(),
        2,
        &[("2026-10-10T12:00:06Z", "singleton starting")],
    );

    let report = check_dir(dir.path(), "counter").unwrap();

    assert!(!report.passed());
    assert_eq!(report.overlaps, vec![(base + 9_000_000, base + 9_050_000)]);
    assert!(
        report.render().contains("OVERLAP +9.000s to +9.050s (0.050 s): both nodes ran the singleton, from when node 1 was continued after a pause"),
        "{}",
        report.render()
    );
}

#[test]
fn check_dir____old_primary_thawed_after_its_lease_ended____no_overlap() {
    let dir = tempdir();
    let base = parse_rfc3339_us("2026-10-10T12:00:00Z").unwrap();
    write_events(
        dir.path(),
        &[
            (base, 1, What::Spawn),
            (base, 2, What::Spawn),
            (base + 2_000_000, 1, What::Freeze),
            (base + 9_000_000, 1, What::Thaw),
        ],
    );
    write_log(
        dir.path(),
        1,
        &[
            ("2026-10-10T12:00:01Z", "singleton starting"),
            (
                "2026-10-10T12:00:09.040Z",
                "failover lease ended ended_ms_ago=6040",
            ),
            ("2026-10-10T12:00:09.050Z", "singleton killed"),
        ],
    );
    write_log(
        dir.path(),
        2,
        &[("2026-10-10T12:00:06Z", "singleton starting")],
    );

    let report = check_dir(dir.path(), "counter").unwrap();

    assert!(report.passed(), "{}", report.render());
}

#[test]
fn check_dir____callback_cut_off_after_the_thaw____overlap_until_the_cut() {
    let dir = tempdir();
    let base = parse_rfc3339_us("2026-10-10T12:00:00Z").unwrap();
    write_events(
        dir.path(),
        &[
            (base, 1, What::Spawn),
            (base, 2, What::Spawn),
            (base + 2_000_000, 1, What::Freeze),
            (base + 9_000_000, 1, What::Thaw),
        ],
    );
    write_log(
        dir.path(),
        1,
        &[
            ("2026-10-10T12:00:01Z", "singleton starting"),
            (
                "2026-10-10T12:00:09.001Z",
                "instance failed reason=crashed: lease_ended",
            ),
            (
                "2026-10-10T12:00:09.040Z",
                "failover lease ended ended_ms_ago=6040",
            ),
            ("2026-10-10T12:00:09.050Z", "singleton killed"),
        ],
    );
    write_log(
        dir.path(),
        2,
        &[("2026-10-10T12:00:06Z", "singleton starting")],
    );

    let report = check_dir(dir.path(), "counter").unwrap();

    assert_eq!(report.overlaps, vec![(base + 9_000_000, base + 9_001_000)]);
}

#[test]
fn check_dir____paused_past_its_lease_then_killed_at_once____no_overlap() {
    let dir = tempdir();
    let base = parse_rfc3339_us("2026-10-10T12:00:00Z").unwrap();
    write_events(
        dir.path(),
        &[
            (base, 1, What::Spawn),
            (base, 2, What::Spawn),
            (base + 2_000_000, 1, What::Freeze),
            (base + 9_000_000, 1, What::Thaw),
            (base + 9_006_000, 1, What::Kill),
        ],
    );
    write_log(
        dir.path(),
        1,
        &[
            ("2026-10-10T12:00:01Z", "singleton starting"),
            (
                "2026-10-10T12:00:01.900Z",
                "failover lease extended until_ms=2400 slack_ms=500",
            ),
        ],
    );
    write_log(
        dir.path(),
        2,
        &[("2026-10-10T12:00:06Z", "singleton starting")],
    );

    let report = check_dir(dir.path(), "counter").unwrap();

    assert!(report.passed(), "{}", report.render());
}

#[test]
fn running____lease_bound_after_the_stop____ends_at_the_stop() {
    let base = parse_rfc3339_us("2026-10-10T12:00:00Z").unwrap();
    let records = vec![
        rec(base + 1_000_000, 1, Mark::Start),
        rec(base + 1_500_000, 1, Mark::LeaseBound(base + 9_000_000)),
        rec(base + 4_000_000, 1, Mark::Stop),
    ];

    let runs = running(&records, 1, base + 20_000_000);

    assert_eq!(runs, vec![(base + 1_000_000, base + 4_000_000)]);
}

#[test]
fn check_dir____two_runners_after_a_heal____unsettled() {
    let dir = tempdir();
    let base = parse_rfc3339_us("2026-10-10T12:00:00Z").unwrap();
    write_events(dir.path(), &[(base, 0, What::Settled { runners: vec![] })]);
    write_log(
        dir.path(),
        1,
        &[
            ("2026-10-10T11:59:59Z", "singleton starting"),
            ("2026-10-10T11:59:59.5Z", "singleton killed"),
        ],
    );
    write_log(dir.path(), 2, &[]);

    let report = check_dir(dir.path(), "counter").unwrap();

    assert!(!report.passed());
    assert_eq!(report.unsettled, vec![(base, vec![])]);
}

#[test]
fn check_dir____no_gate_records____fails_as_unchecked() {
    let dir = tempdir();
    write_events(dir.path(), &[(1, 1, What::Spawn)]);
    write_log(dir.path(), 1, &[]);
    write_log(dir.path(), 2, &[]);

    let report = check_dir(dir.path(), "counter").unwrap();

    assert!(!report.passed());
    assert!(report.render().contains("nothing was checked"));
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn write_events(dir: &Path, events: &[(i64, u32, What)]) {
    let log = events::EventLog::create(&dir.join("events.jsonl")).unwrap();
    for (at, node, what) in events {
        log.record_at(*at, *node, what.clone());
    }
}

fn write_log(dir: &Path, node: u32, lines: &[(&str, &str)]) {
    let text: String = lines
        .iter()
        .map(|(at, what)| format!("{at}  INFO paddock::node::gate: {what}: x app=counter\n"))
        .collect();
    std::fs::write(dir.join(format!("node-{node}.log")), text).unwrap();
}
