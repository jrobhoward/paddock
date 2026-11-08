#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[test]
fn InterfaceName____full_name____splits_parts() {
    let name = InterfaceName::parse("example:greeting/greet@0.1.0").unwrap();

    assert_eq!(name.package, "example:greeting");
    assert_eq!(name.interface, Some("greet"));
    assert_eq!(name.version, Some("0.1.0"));
}

#[test]
fn Capability____every_key____round_trips_through_from_key() {
    for capability in Capability::ALL {
        let parsed = Capability::from_key(capability.key());

        assert_eq!(parsed, Some(*capability));
    }
}

#[test]
fn Capability____every_key____is_a_field_of_capabilities_section() {
    for capability in Capability::ALL {
        let value = serde_json::json!({ capability.key(): minimal_scope(*capability) });

        let section: Capabilities = serde_json::from_value(value).unwrap();

        assert_eq!(section.declared(), vec![*capability]);
    }
}

fn minimal_scope(capability: Capability) -> serde_json::Value {
    match capability {
        Capability::Sql => serde_json::json!({ "datasources": { "db": { "max_connections": 1 } } }),
        Capability::Metrics => serde_json::json!({ "prefix": "app" }),
        _ => serde_json::json!({}),
    }
}

#[rstest]
#[case("10.0.0.0/8")]
#[case("127.0.0.1/32")]
#[case("0.0.0.0/0")]
#[case("::1/128")]
#[case("fd00::/8")]
fn parse_cidr____valid____returns_some(#[case] cidr: &str) {
    assert!(parse_cidr(cidr).is_some());
}

#[rstest]
#[case("10.0.0.0")]
#[case("10.0.0.0/33")]
#[case("::1/129")]
#[case("example.com/8")]
#[case("10.0.0.0/x")]
fn parse_cidr____invalid____returns_none(#[case] cidr: &str) {
    assert!(parse_cidr(cidr).is_none());
}

#[rstest]
#[case("443", 443, 443)]
#[case("8000-8100", 8000, 8100)]
#[case("0-65535", 0, 65535)]
fn PortRange____valid____parses(#[case] s: &str, #[case] start: u16, #[case] end: u16) {
    let range: PortRange = s.parse().unwrap();

    assert_eq!((range.start, range.end), (start, end));
    assert_eq!(range.to_string(), s);
}

#[rstest]
#[case("")]
#[case("443-80")]
#[case("70000")]
#[case("a-b")]
fn PortRange____invalid____returns_error(#[case] s: &str) {
    assert!(s.parse::<PortRange>().is_err());
}

#[rstest]
#[case("/data", true)]
#[case("/data/cache", true)]
#[case("/", false)]
#[case("data", false)]
#[case("/data/", false)]
#[case("/data//x", false)]
#[case("/data/./x", false)]
#[case("/data/../etc", false)]
fn is_valid_guest_path____cases____match(#[case] path: &str, #[case] expected: bool) {
    assert_eq!(is_valid_guest_path(path), expected);
}

#[rstest]
#[case("10.0.0.0/24", "10.0.0.17", true)]
#[case("10.0.0.0/24", "10.0.1.17", false)]
#[case("127.0.0.1/32", "127.0.0.1", true)]
#[case("0.0.0.0/0", "192.0.2.1", true)]
#[case("fd00::/8", "fd12::1", true)]
#[case("fd00::/8", "fe80::1", false)]
#[case("10.0.0.0/8", "::ffff:10.0.0.1", false)]
fn cidr_contains____address____inside_or_not(
    #[case] net: &str,
    #[case] ip: &str,
    #[case] inside: bool,
) {
    let net = parse_cidr(net).unwrap();

    let found = cidr_contains(net, ip.parse().unwrap());

    assert_eq!(found, inside);
}
