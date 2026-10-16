#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case("paddock:runtime/types@0.1.0")]
#[case("paddock:runtime/process@0.1.0")]
#[case("wasi:clocks/monotonic-clock@0.2.12")]
#[case("wasi:random/random@0.2.12")]
#[case("wasi:io/streams@0.2.12")]
#[case("wasi:cli/stdout@0.2.12")]
#[case("wasi:cli/environment")]
fn classify_import____implicit_interface____returns_implicit(#[case] name: &str) {
    let class = classify_import(name);

    assert_eq!(class, ImportClass::Implicit);
}

#[rstest]
#[case("wasi:logging/logging@0.1.0-draft", Capability::Logging)]
#[case("wasi:config/store@0.2.0-draft", Capability::Config)]
#[case("wasi:keyvalue/store@0.2.0-draft", Capability::KeyValue)]
#[case("wasi:keyvalue/atomics@0.2.0-draft", Capability::KeyValue)]
#[case("wasi:filesystem/preopens@0.2.12", Capability::Filesystem)]
#[case("wasi:filesystem/types@0.2.12", Capability::Filesystem)]
#[case("wasi:sockets/tcp@0.2.12", Capability::Sockets)]
#[case("wasi:http/outgoing-handler@0.2.12", Capability::HttpOutgoing)]
#[case("wasi:http/incoming-handler@0.2.12", Capability::HttpIncoming)]
#[case("paddock:runtime/supervisor@0.1.0", Capability::Supervisor)]
#[case("paddock:secrets/store@0.1.0", Capability::Secrets)]
fn classify_import____capability_interface____returns_its_capability(
    #[case] name: &str,
    #[case] expected: Capability,
) {
    let class = classify_import(name);

    let ImportClass::Capability(capabilities) = class else {
        panic!("expected Capability, got {class:?}");
    };
    assert_eq!(capabilities, [expected]);
}

#[test]
fn classify_import____http_types____shared_by_both_http_capabilities() {
    let class = classify_import("wasi:http/types@0.2.12");

    assert_eq!(
        class,
        ImportClass::Capability(&[Capability::HttpOutgoing, Capability::HttpIncoming])
    );
}

#[rstest]
#[case("paddock:spike4/kv@0.1.0")]
#[case("paddock:runtime/admin@0.1.0")]
#[case("wasi:nn/inference@0.2.0")]
#[case("log")]
#[case(":x/y")]
fn classify_import____no_host_provides_it____returns_unknown(#[case] name: &str) {
    let class = classify_import(name);

    assert_eq!(class, ImportClass::Unknown);
}

#[test]
fn InterfaceName____full_name____splits_parts() {
    let name = InterfaceName::parse("wasi:keyvalue/store@0.2.0-draft").unwrap();

    assert_eq!(name.package, "wasi:keyvalue");
    assert_eq!(name.interface, Some("store"));
    assert_eq!(name.version, Some("0.2.0-draft"));
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

#[test]
fn classify_import____paddock_watch____needs_keyvalue() {
    let class = classify_import("paddock:watch/watch@0.1.0");

    assert_eq!(class, ImportClass::Capability(&[Capability::KeyValue]));
    assert_eq!(Capability::ALL[KEYVALUE], Capability::KeyValue);
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
