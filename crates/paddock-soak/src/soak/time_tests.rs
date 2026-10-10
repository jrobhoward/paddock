#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn parse_rfc3339_us____epoch____zero() {
    let at = parse_rfc3339_us("1970-01-01T00:00:00Z");

    assert_eq!(at, Some(0));
}

#[test]
fn parse_rfc3339_us____tracing_timestamp____microseconds() {
    let at = parse_rfc3339_us("2026-10-10T12:34:56.123456Z");

    // date -u -d 2026-10-10T12:34:56Z +%s
    assert_eq!(at, Some(1_791_635_696_123_456));
}

#[test]
fn parse_rfc3339_us____short_fraction____scaled() {
    let at = parse_rfc3339_us("2000-03-01T00:00:00.5Z");

    assert_eq!(at, Some(951_868_800_500_000));
}

#[test]
fn parse_rfc3339_us____not_a_time____none() {
    let cases = [
        "",
        "INFO",
        "2026-10-10 12:34:56Z",
        "2026-13-10T12:34:56Z",
        "2026-10-10T12:34:56.1x2Z",
    ];

    let parsed: Vec<Option<i64>> = cases.iter().map(|c| parse_rfc3339_us(c)).collect();

    assert!(parsed.iter().all(Option::is_none), "{parsed:?}");
}

#[test]
fn now_us____after_2026____positive() {
    let now = now_us();

    assert!(now > 1_767_225_600_000_000);
}
