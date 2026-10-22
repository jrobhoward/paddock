#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn points(at_ms: u64, node: u32, counter: Option<u64>) -> Sample {
    Sample {
        at_ms,
        node,
        ask: Ask::Points,
        ok: counter.is_some(),
        counter,
    }
}

fn alarms(at_ms: u64, node: u32, ok: bool) -> Sample {
    Sample {
        at_ms,
        node,
        ask: Ask::Alarms,
        ok,
        counter: None,
    }
}

fn window(kind: Kind, start_ms: u64, end_ms: u64) -> Window {
    Window {
        name: "w".into(),
        kind,
        start_ms,
        end_ms,
    }
}

#[test]
fn tally____counter_stalls_then_moves____longest_gap_from_the_change_before() {
    let samples = [
        points(900, 1, Some(9)),
        points(1000, 1, Some(10)),
        points(1100, 1, Some(10)),
        points(1700, 1, Some(10)),
        points(1800, 1, Some(17)),
        points(1900, 2, Some(1)),
        points(2000, 1, Some(18)),
    ];

    let t = tally(&samples, 1, &window(Kind::Release, 1050, 2100));

    assert_eq!(t.points, 4);
    assert_eq!(t.points_failed, 0);
    assert_eq!(t.gap_ms, 800);
}

#[test]
fn tally____failed_reads_and_alarms____counted_and_the_gap_restarts() {
    let samples = [
        points(0, 1, Some(1)),
        points(100, 1, None),
        alarms(150, 1, false),
        alarms(400, 1, true),
        alarms(650, 1, false),
        points(2000, 1, Some(20)),
        points(2100, 1, Some(21)),
    ];

    let t = tally(&samples, 1, &window(Kind::Switchover, 0, 3000));

    assert_eq!((t.points, t.points_failed), (4, 1));
    assert_eq!((t.alarms, t.alarms_failed), (3, 2));
    assert_eq!(t.alarm_span_ms, 500);
    assert_eq!(t.gap_ms, 100);
}

#[rstest::rstest]
#[case::clean(Kind::Release, Tally { points: 30, alarms: 30, gap_ms: 200, ..Tally::default() }, 0)]
#[case::point_failed(Kind::Release, Tally { points: 30, points_failed: 1, alarms: 30, gap_ms: 200, ..Tally::default() }, 1)]
#[case::gap(Kind::Switchover, Tally { points: 30, alarms: 30, gap_ms: 1000, ..Tally::default() }, 1)]
#[case::alarm_in_release(Kind::Release, Tally { points: 30, alarms: 30, alarms_failed: 1, gap_ms: 200, ..Tally::default() }, 1)]
#[case::handover_short(Kind::Switchover, Tally { points: 30, alarms: 30, alarms_failed: 6, alarm_span_ms: 600, gap_ms: 600, ..Tally::default() }, 0)]
#[case::handover_long(Kind::Switchover, Tally { points: 30, alarms: 30, alarms_failed: 20, alarm_span_ms: 2500, gap_ms: 600, ..Tally::default() }, 1)]
#[case::fault(Kind::Fault, Tally { points: 30, points_failed: 30, gap_ms: 9000, ..Tally::default() }, 0)]
fn judge____planned_or_fault____fails_only_past_the_limits(
    #[case] kind: Kind,
    #[case] t: Tally,
    #[case] problems: usize,
) {
    let why = judge(kind, &t);

    assert_eq!(why.len(), problems, "{why:?}");
}

#[test]
fn parse_reply____plain_and_chunked____status_and_body() {
    let plain = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}";
    let chunked = "HTTP/1.1 503 Service Unavailable\r\nTransfer-Encoding: chunked\r\n\r\n\
                   4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";

    let replies = [
        parse_reply(plain),
        parse_reply(chunked),
        parse_reply("garbage"),
    ];

    assert_eq!(replies[0], Some((200, "{}".into())));
    assert_eq!(replies[1], Some((503, "{\"a\":1}".into())));
    assert_eq!(replies[2], None);
}

#[test]
fn counter_and_alarm_status____replies_of_the_apps____read() {
    let points = r#"{"revision":4,"points":{"pump-1/holding/0":17},"devices":{}}"#;
    let alarms = r#"{"version":"2.0.0","alarms":{"pump-1-pressure":{"status":"acked"}}}"#;

    let read = (counter(points), alarm_status(alarms), counter("{}"));

    assert_eq!(read, (Some(17), Some("acked".into()), None));
}
