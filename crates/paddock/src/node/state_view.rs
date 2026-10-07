//! Reading an instance's snapshot for `paddock state`.

use crate::api::StateResponse;
use serde_json::Value as Json;

/// Describe snapshot bytes: the SDK's `[format_version, state]` CBOR pair,
/// JSON, UTF-8 text, other CBOR, or raw bytes as hex. Text is tried before
/// generic CBOR because short text (`"4"`) is also valid CBOR. CBOR must
/// use every byte, and text must have no control characters but
/// whitespace, so binary snapshots are not mistaken for either.
#[must_use]
pub fn describe(pid: &str, bytes: &[u8]) -> StateResponse {
    let mut out = StateResponse {
        pid: pid.to_string(),
        bytes: bytes.len(),
        format: "raw".into(),
        format_version: None,
        value: None,
        hex: None,
    };
    if bytes.is_empty() {
        out.format = "empty".into();
        return out;
    }
    if let Some(ciborium::Value::Array(items)) = cbor_exact(bytes)
        && let [ciborium::Value::Integer(v), state] = items.as_slice()
        && let Ok(version) = u64::try_from(i128::from(*v))
    {
        out.format = "paddock-sdk".into();
        out.format_version = Some(version);
        out.value = Some(to_json(state));
        return out;
    }
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text.chars().any(|c| c.is_control() && !c.is_whitespace())
    {
        if let Ok(json) = serde_json::from_str::<Json>(text) {
            out.format = "json".into();
            out.value = Some(json);
        } else {
            out.format = "text".into();
            out.value = Some(Json::String(text.to_string()));
        }
        return out;
    }
    if let Some(value) = cbor_exact(bytes) {
        out.format = "cbor".into();
        out.value = Some(to_json(&value));
        return out;
    }
    out.hex = Some(bytes.iter().map(|b| format!("{b:02x}")).collect());
    out
}

/// One CBOR item spanning all of `bytes`.
fn cbor_exact(bytes: &[u8]) -> Option<ciborium::Value> {
    let mut cursor = std::io::Cursor::new(bytes);
    let value = ciborium::from_reader::<ciborium::Value, _>(&mut cursor).ok()?;
    (usize::try_from(cursor.position()).ok()? == bytes.len()).then_some(value)
}

/// CBOR to JSON for display: byte strings become hex, map keys become
/// strings, tags are dropped.
fn to_json(value: &ciborium::Value) -> Json {
    use ciborium::Value as C;
    match value {
        C::Integer(i) => {
            let n = i128::from(*i);
            i64::try_from(n)
                .map(Json::from)
                .or_else(|_| u64::try_from(n).map(Json::from))
                .unwrap_or_else(|_| Json::String(n.to_string()))
        }
        C::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        C::Text(s) => Json::String(s.clone()),
        C::Bytes(b) => Json::String(b.iter().map(|x| format!("{x:02x}")).collect()),
        C::Bool(b) => Json::Bool(*b),
        C::Null => Json::Null,
        C::Tag(_, inner) => to_json(inner),
        C::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        C::Map(entries) => Json::Object(
            entries
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        C::Text(s) => s.clone(),
                        other => to_json(other).to_string(),
                    };
                    (key, to_json(v))
                })
                .collect(),
        ),
        _ => Json::Null,
    }
}

#[cfg(test)]
#[path = "state_view_tests.rs"]
mod state_view_tests;
