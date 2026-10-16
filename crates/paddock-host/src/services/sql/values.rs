//! Values between guests and Postgres. Parameters go as text, in the form
//! the server's input functions read, so the server converts each to the
//! type its statement needs. Columns arrive in the binary format and are
//! decoded here for the types `paddock:sql` carries; dates, times, `numeric`,
//! `json`, `jsonb` and `uuid` become their text form, and anything else is
//! refused with the type's name, to be cast in the statement.

use tokio_postgres::types::{FromSql, Type};

/// A parameter or a column's value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl Value {
    /// Bytes it adds to a result, for `max_result_bytes`.
    pub(crate) fn size(&self) -> u64 {
        let payload = match self {
            Self::Null | Self::Bool(_) | Self::Int(_) | Self::Float(_) => 0,
            Self::Text(s) => s.len(),
            Self::Bytes(b) => b.len(),
        };
        8 + u64::try_from(payload).unwrap_or(u64::MAX)
    }
}

/// The parameter's text form; `None` for null.
pub(crate) fn param_text(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(if *b { b"t".to_vec() } else { b"f".to_vec() }),
        Value::Int(i) => Some(i.to_string().into_bytes()),
        Value::Float(f) if f.is_nan() => Some(b"NaN".to_vec()),
        Value::Float(f) if f.is_infinite() => Some(if *f > 0.0 {
            b"Infinity".to_vec()
        } else {
            b"-Infinity".to_vec()
        }),
        Value::Float(f) => Some(f.to_string().into_bytes()),
        Value::Text(s) => Some(s.clone().into_bytes()),
        Value::Bytes(b) => {
            let mut out = Vec::with_capacity(2 + b.len() * 2);
            out.extend_from_slice(b"\\x");
            for byte in b {
                out.extend_from_slice(format!("{byte:02x}").as_bytes());
            }
            Some(out)
        }
    }
}

/// Why a column could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// A type this interface does not carry.
    Unsupported(String),
    /// The server sent bytes that are not that type's binary form.
    Malformed(String),
}

/// Decode a column of type `ty`, `None` for null.
pub(crate) fn decode(ty: &Type, raw: Option<&[u8]>) -> Result<Value, DecodeError> {
    let Some(raw) = raw else {
        return Ok(Value::Null);
    };
    let bad = || DecodeError::Malformed(format!("{} value of {} bytes", ty.name(), raw.len()));
    let fixed = |n: usize| -> Result<&[u8], DecodeError> {
        if raw.len() == n { Ok(raw) } else { Err(bad()) }
    };
    let text = |bytes: &[u8]| {
        String::from_utf8(bytes.to_vec())
            .map(Value::Text)
            .map_err(|_| bad())
    };
    Ok(match *ty {
        Type::BOOL => Value::Bool(fixed(1)?[0] != 0),
        Type::INT2 => Value::Int(i64::from(i16::from_be_bytes(array(fixed(2)?)))),
        Type::INT4 => Value::Int(i64::from(i32::from_be_bytes(array(fixed(4)?)))),
        Type::INT8 => Value::Int(i64::from_be_bytes(array(fixed(8)?))),
        Type::OID => Value::Int(i64::from(u32::from_be_bytes(array(fixed(4)?)))),
        Type::FLOAT4 => Value::Float(f64::from(f32::from_be_bytes(array(fixed(4)?)))),
        Type::FLOAT8 => Value::Float(f64::from_be_bytes(array(fixed(8)?))),
        Type::BYTEA => Value::Bytes(raw.to_vec()),
        Type::JSON => text(raw)?,
        Type::JSONB => match raw.split_first() {
            Some((1, json)) => text(json)?,
            _ => return Err(bad()),
        },
        Type::UUID => Value::Text(uuid(fixed(16)?)),
        Type::NUMERIC => Value::Text(numeric(raw).ok_or_else(bad)?),
        Type::DATE => Value::Text(date(i32::from_be_bytes(array(fixed(4)?)))),
        Type::TIMESTAMP => Value::Text(timestamp(i64::from_be_bytes(array(fixed(8)?)), "")),
        Type::TIMESTAMPTZ => Value::Text(timestamp(i64::from_be_bytes(array(fixed(8)?)), "+00")),
        Type::TIME => Value::Text(time(i64::from_be_bytes(array(fixed(8)?)))),
        // Text and the types stored as text (`varchar`, `name`, `citext`...).
        _ if <String as FromSql>::accepts(ty) => text(raw)?,
        _ => return Err(DecodeError::Unsupported(ty.name().to_string())),
    })
}

fn array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut out = [0; N];
    out.copy_from_slice(&bytes[..N]);
    out
}

fn uuid(b: &[u8]) -> String {
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `numeric`'s binary form: digit count, weight (of the first digit, in
/// base-10000 groups), sign, display scale, then the base-10000 digits.
fn numeric(raw: &[u8]) -> Option<String> {
    let word = |i: usize| -> Option<u16> {
        raw.get(i * 2..i * 2 + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
    };
    let ndigits = usize::from(word(0)?);
    let weight = i64::from(word(1)?.cast_signed());
    let sign = word(2)?;
    let dscale = usize::from(word(3)?);
    match sign {
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        0x0000 | 0x4000 => {}
        _ => return None,
    }
    let digits: Vec<u16> = (0..ndigits).map(|i| word(4 + i)).collect::<Option<_>>()?;
    if digits.iter().any(|d| *d > 9999) {
        return None;
    }
    let group = |g: i64| -> u16 {
        usize::try_from(g)
            .ok()
            .and_then(|g| digits.get(g).copied())
            .unwrap_or(0)
    };
    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        out.push_str(&group(0).to_string());
        for g in 1..=weight {
            out.push_str(&format!("{:04}", group(g)));
        }
    }
    if dscale > 0 {
        out.push('.');
        let mut frac = String::new();
        let mut g = weight + 1;
        while frac.len() < dscale {
            frac.push_str(&format!("{:04}", group(g)));
            g += 1;
        }
        frac.truncate(dscale);
        out.push_str(&frac);
    }
    Some(out)
}

/// Days since 2000-01-01 to (year, month, day), proleptic Gregorian
/// (Howard Hinnant's `civil_from_days`).
fn civil(days_since_2000: i64) -> (i64, u32, u32) {
    let z = days_since_2000 + 10_957 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn ymd(days: i64) -> String {
    let (y, m, d) = civil(days);
    if y <= 0 {
        // Postgres writes 1 BC for year 0.
        format!("{:04}-{m:02}-{d:02} BC", 1 - y)
    } else {
        format!("{y:04}-{m:02}-{d:02}")
    }
}

fn date(days: i32) -> String {
    match days {
        i32::MAX => "infinity".into(),
        i32::MIN => "-infinity".into(),
        d => ymd(i64::from(d)),
    }
}

/// Microseconds in a day.
const DAY_US: i64 = 86_400_000_000;

fn clock(us: i64) -> String {
    let (h, rest) = (us / 3_600_000_000, us % 3_600_000_000);
    let (m, rest) = (rest / 60_000_000, rest % 60_000_000);
    let (s, frac) = (rest / 1_000_000, rest % 1_000_000);
    if frac == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        let frac = format!("{frac:06}");
        format!("{h:02}:{m:02}:{s:02}.{}", frac.trim_end_matches('0'))
    }
}

fn timestamp(us: i64, zone: &str) -> String {
    match us {
        i64::MAX => "infinity".into(),
        i64::MIN => "-infinity".into(),
        us => {
            let days = us.div_euclid(DAY_US);
            let time = us.rem_euclid(DAY_US);
            let date = ymd(days);
            // `BC` goes after the time, as Postgres writes it.
            match date.strip_suffix(" BC") {
                Some(d) => format!("{d} {}{zone} BC", clock(time)),
                None => format!("{date} {}{zone}", clock(time)),
            }
        }
    }
}

fn time(us: i64) -> String {
    clock(us.rem_euclid(DAY_US))
}

#[cfg(test)]
#[path = "values_tests.rs"]
mod values_tests;
