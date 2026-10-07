#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use proptest::prelude::*;
use rstest::rstest;

use super::*;

#[rstest]
#[case("0", 0)]
#[case("100", 100)]
#[case("100B", 100)]
#[case("64MiB", 64 * 1024 * 1024)]
#[case("64 MiB", 64 * 1024 * 1024)]
#[case("1GiB", 1 << 30)]
#[case("2TiB", 2 << 40)]
#[case("512KiB", 512 * 1024)]
#[case("500MB", 500_000_000)]
#[case("1KB", 1000)]
#[case(" 5GB ", 5_000_000_000)]
fn ByteSize____parse_valid____returns_bytes(#[case] input: &str, #[case] expected: u64) {
    let parsed: ByteSize = input.parse().unwrap();

    assert_eq!(parsed.bytes(), expected);
}

#[rstest]
#[case("")]
#[case("MiB")]
#[case("64mib")]
#[case("64 XB")]
#[case("1.5GiB")]
#[case("-1")]
#[case("99999999999999999999")]
#[case("17179869184GiB")]
fn ByteSize____parse_invalid____returns_error(#[case] input: &str) {
    let parsed = input.parse::<ByteSize>();

    assert!(parsed.is_err());
}

#[rstest]
#[case(ByteSize::mib(64), "64MiB")]
#[case(ByteSize::kib(1536), "1536KiB")]
#[case(ByteSize::gib(1), "1GiB")]
#[case(ByteSize(100), "100B")]
#[case(ByteSize(0), "0B")]
#[case(ByteSize(500_000_000), "500000000B")]
fn ByteSize____display____uses_largest_exact_binary_unit(
    #[case] size: ByteSize,
    #[case] expected: &str,
) {
    let shown = size.to_string();

    assert_eq!(shown, expected);
}

#[test]
fn ByteSize____deserialize_integer____reads_bytes() {
    let size: ByteSize = serde_json::from_str("4096").unwrap();

    assert_eq!(size, ByteSize::kib(4));
}

#[test]
fn ByteSize____deserialize_negative____returns_error() {
    let size = serde_json::from_str::<ByteSize>("-1");

    assert!(size.is_err());
}

#[test]
fn ByteSize____serialize____writes_string() {
    let json = serde_json::to_string(&ByteSize::mib(64)).unwrap();

    assert_eq!(json, "\"64MiB\"");
}

proptest! {
    #[test]
    fn ByteSize____display_then_parse____round_trips(bytes in any::<u64>()) {
        let size = ByteSize(bytes);

        let parsed: ByteSize = size.to_string().parse().unwrap();

        prop_assert_eq!(parsed, size);
    }
}
