#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

fn numeric_bytes(weight: i16, sign: u16, dscale: u16, digits: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    for word in [
        u16::try_from(digits.len()).unwrap(),
        weight.cast_unsigned(),
        sign,
        dscale,
    ] {
        out.extend_from_slice(&word.to_be_bytes());
    }
    for d in digits {
        out.extend_from_slice(&d.to_be_bytes());
    }
    out
}

#[rstest]
#[case(Value::Null, None)]
#[case(Value::Bool(true), Some("t"))]
#[case(Value::Int(-42), Some("-42"))]
#[case(Value::Float(1.5), Some("1.5"))]
#[case(Value::Float(f64::NEG_INFINITY), Some("-Infinity"))]
#[case(Value::Text("héllo".into()), Some("héllo"))]
#[case(Value::Bytes(vec![0, 0xab, 0xff]), Some("\\x00abff"))]
fn param_text____each_kind____postgres_input_form(#[case] v: Value, #[case] want: Option<&str>) {
    let got = param_text(&v);

    assert_eq!(got.as_deref(), want.map(str::as_bytes));
}

#[rstest]
#[case(Type::BOOL, vec![1], Value::Bool(true))]
#[case(Type::INT2, (-2i16).to_be_bytes().to_vec(), Value::Int(-2))]
#[case(Type::INT4, 70_000i32.to_be_bytes().to_vec(), Value::Int(70_000))]
#[case(Type::INT8, i64::MIN.to_be_bytes().to_vec(), Value::Int(i64::MIN))]
#[case(Type::FLOAT4, 0.5f32.to_be_bytes().to_vec(), Value::Float(0.5))]
#[case(Type::FLOAT8, 2.25f64.to_be_bytes().to_vec(), Value::Float(2.25))]
#[case(Type::TEXT, b"hi".to_vec(), Value::Text("hi".into()))]
#[case(Type::VARCHAR, b"v".to_vec(), Value::Text("v".into()))]
#[case(Type::BYTEA, vec![1, 2], Value::Bytes(vec![1, 2]))]
#[case(Type::JSON, br#"{"a":1}"#.to_vec(), Value::Text(r#"{"a":1}"#.into()))]
#[case(Type::JSONB, [&[1u8][..], br#"[1]"#].concat(), Value::Text("[1]".into()))]
fn decode____scalar_types____carried_values(
    #[case] ty: Type,
    #[case] raw: Vec<u8>,
    #[case] want: Value,
) {
    let got = decode(&ty, Some(&raw));

    assert_eq!(got, Ok(want));
}

#[test]
fn decode____null_of_any_type____null() {
    let got = decode(&Type::INT4, None);

    assert_eq!(got, Ok(Value::Null));
}

#[test]
fn decode____uuid____hyphenated_text() {
    let raw: Vec<u8> = (0u8..16).collect();

    let got = decode(&Type::UUID, Some(&raw));

    assert_eq!(
        got,
        Ok(Value::Text("00010203-0405-0607-0809-0a0b0c0d0e0f".into()))
    );
}

#[rstest]
// 12345.678: groups 1|2345|6780, weight 1, scale 3.
#[case(numeric_bytes(1, 0, 3, &[1, 2345, 6780]), "12345.678")]
#[case(numeric_bytes(0, 0x4000, 2, &[7, 5000]), "-7.50")]
// 0.0001: one group 1 at weight -1.
#[case(numeric_bytes(-1, 0, 4, &[1]), "0.0001")]
// 0.00000002: group 2 at weight -2 (0.0000|0002).
#[case(numeric_bytes(-2, 0, 8, &[2]), "0.00000002")]
#[case(numeric_bytes(2, 0, 0, &[1]), "100000000")]
#[case(numeric_bytes(0, 0, 0, &[]), "0")]
#[case(numeric_bytes(0, 0, 2, &[]), "0.00")]
#[case(numeric_bytes(0, 0xC000, 0, &[]), "NaN")]
fn decode____numeric____exact_text(#[case] raw: Vec<u8>, #[case] want: &str) {
    let got = decode(&Type::NUMERIC, Some(&raw));

    assert_eq!(got, Ok(Value::Text(want.into())));
}

#[rstest]
#[case(0, "2000-01-01")]
#[case(-1, "1999-12-31")]
#[case(9_190, "2025-02-28")]
#[case(9_191, "2025-03-01")]
#[case(-730_119, "0001-01-01")]
#[case(-730_120, "0001-12-31 BC")]
#[case(i32::MAX, "infinity")]
fn decode____date____iso(#[case] days: i32, #[case] want: &str) {
    let got = decode(&Type::DATE, Some(&days.to_be_bytes()));

    assert_eq!(got, Ok(Value::Text(want.into())));
}

#[rstest]
#[case(Type::TIMESTAMP, 0, "2000-01-01 00:00:00")]
#[case(Type::TIMESTAMPTZ, 86_400_000_000 + 3_723_500_000, "2000-01-02 01:02:03.5+00")]
#[case(Type::TIMESTAMP, -1, "1999-12-31 23:59:59.999999")]
#[case(Type::TIME, 45_296_000_001, "12:34:56.000001")]
fn decode____timestamps_and_times____iso(#[case] ty: Type, #[case] us: i64, #[case] want: &str) {
    let got = decode(&ty, Some(&us.to_be_bytes()));

    assert_eq!(got, Ok(Value::Text(want.into())));
}

#[test]
fn decode____interval____unsupported_names_the_type() {
    let got = decode(&Type::INTERVAL, Some(&[0; 16]));

    assert_eq!(got, Err(DecodeError::Unsupported("interval".into())));
}

#[test]
fn decode____wrong_length____malformed() {
    let got = decode(&Type::INT4, Some(&[1, 2]));

    assert!(matches!(got, Err(DecodeError::Malformed(_))), "{got:?}");
}
