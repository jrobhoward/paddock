//! Wall-clock times in microseconds since the Unix epoch. The runner and
//! the nodes share one machine, so their clocks agree.

use std::time::{SystemTime, UNIX_EPOCH};

/// Now, in microseconds since the epoch.
pub(crate) fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

/// An RFC 3339 UTC time as `tracing` writes it
/// (`2026-10-10T12:34:56.123456Z`), in microseconds since the epoch.
pub(crate) fn parse_rfc3339_us(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut d = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let (hms, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.splitn(3, ':').map(str::parse::<i64>);
    let (hour, minute, second) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut micros = 0;
    for (i, b) in fraction.bytes().take(6).enumerate() {
        micros += i64::from(b - b'0') * 10_i64.pow(5 - u32::try_from(i).ok()?);
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    Some(seconds * 1_000_000 + micros)
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
#[path = "time_tests.rs"]
mod time_tests;
