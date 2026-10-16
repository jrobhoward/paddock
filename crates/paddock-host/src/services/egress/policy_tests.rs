#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case::origin("https://api.example.com", Scheme::Https, Host::Name("api.example.com".into()), 443)]
#[case::http_port("http://plc.internal:8080", Scheme::Http, Host::Name("plc.internal".into()), 8080)]
#[case::bare("API.Example.com", Scheme::Https, Host::Name("api.example.com".into()), 443)]
#[case::trailing_slash("https://a.b/", Scheme::Https, Host::Name("a.b".into()), 443)]
#[case::wildcard("*.internal", Scheme::Https, Host::Under("internal".into()), 443)]
#[case::wildcard_http("http://*.plant.internal:81", Scheme::Http, Host::Under("plant.internal".into()), 81)]
#[case::ipv4("http://10.0.0.5:8080", Scheme::Http, Host::Ip("10.0.0.5".parse().unwrap()), 8080)]
#[case::ipv6("https://[fd00::1]", Scheme::Https, Host::Ip("fd00::1".parse().unwrap()), 443)]
#[case::ipv6_port("http://[::1]:9000", Scheme::Http, Host::Ip("::1".parse().unwrap()), 9000)]
fn parse____valid_entry____scheme_host_and_port(
    #[case] entry: &str,
    #[case] scheme: Scheme,
    #[case] host: Host,
    #[case] port: u16,
) {
    assert_eq!(parse(entry).unwrap(), Allow { scheme, host, port });
}

#[rstest]
#[case::path("https://a.example/api")]
#[case::query("https://a.example?x=1")]
#[case::user("https://me@a.example")]
#[case::scheme("ftp://a.example")]
#[case::star("*")]
#[case::star_scheme("https://*")]
#[case::port_zero("a.example:0")]
#[case::port_big("a.example:70000")]
#[case::bare_ipv6("::1")]
#[case::underscore("a_b.example")]
#[case::empty("")]
fn parse____invalid_entry____refused(#[case] entry: &str) {
    assert!(parse(entry).is_err(), "{entry}");
}

#[rstest]
#[case::exact("https://api.example.com", Scheme::Https, "api.example.com", 443, true)]
#[case::case("https://api.example.com", Scheme::Https, "API.example.COM", 443, true)]
#[case::trailing_dot(
    "https://api.example.com",
    Scheme::Https,
    "api.example.com.",
    443,
    true
)]
#[case::other_port(
    "https://api.example.com",
    Scheme::Https,
    "api.example.com",
    8443,
    false
)]
#[case::other_scheme("https://api.example.com", Scheme::Http, "api.example.com", 443, false)]
#[case::sub("*.internal", Scheme::Https, "a.b.internal", 443, true)]
#[case::suffix_itself("*.internal", Scheme::Https, "internal", 443, false)]
#[case::lookalike("*.internal", Scheme::Https, "evilinternal", 443, false)]
#[case::name_for_ip("https://localhost", Scheme::Https, "127.0.0.1", 443, false)]
#[case::wildcard_for_ip("*.internal", Scheme::Https, "10.0.0.1", 443, false)]
#[case::ip("http://127.0.0.1:8080", Scheme::Http, "127.0.0.1", 8080, true)]
#[case::ipv6("http://[::1]:8080", Scheme::Http, "[::1]", 8080, true)]
#[case::ip_for_name("http://127.0.0.1:8080", Scheme::Http, "localhost", 8080, false)]
fn allows____request____matches_scheme_host_and_port(
    #[case] entry: &str,
    #[case] scheme: Scheme,
    #[case] host: &str,
    #[case] port: u16,
    #[case] expected: bool,
) {
    assert_eq!(parse(entry).unwrap().allows(scheme, host, port), expected);
}

#[rstest]
#[case::same("https://a.example", "a.example", true)]
#[case::name_under("*.internal", "https://api.internal", true)]
#[case::narrower_wildcard("*.internal", "*.plant.internal", true)]
#[case::same_wildcard("*.internal", "*.internal", true)]
#[case::wider_wildcard("*.plant.internal", "*.internal", false)]
#[case::name_over_wildcard("https://api.internal", "*.internal", false)]
#[case::port("*.internal", "https://api.internal:8443", false)]
#[case::scheme("*.internal", "http://api.internal:443", false)]
#[case::ip("http://10.0.0.5", "http://10.0.0.5:80", true)]
#[case::wildcard_not_ip("*.internal", "https://10.0.0.5", false)]
fn covers____ceiling_and_entry____inside_or_not(
    #[case] ceiling: &str,
    #[case] entry: &str,
    #[case] expected: bool,
) {
    assert_eq!(
        parse(ceiling).unwrap().covers(&parse(entry).unwrap()),
        expected
    );
}

#[test]
fn within____no_ceiling____only_syntax_checked() {
    let ok = within(&["*.internal".into()], None);
    let bad = within(&["https://a/b".into()], None);

    assert!(ok.is_ok());
    assert!(bad.unwrap_err().contains("no path"));
}

#[test]
fn within____entry_outside_ceiling____names_it() {
    let ceiling = vec!["*.internal".to_string()];

    let inside = within(&["https://api.internal".into()], Some(&ceiling));
    let outside = within(
        &["https://api.internal".into(), "api.example.com".into()],
        Some(&ceiling),
    );

    assert!(inside.is_ok());
    assert!(
        outside
            .unwrap_err()
            .contains("\"api.example.com\" is outside"),
        "named"
    );
}

#[rstest]
#[case::v4_loopback("127.0.0.53", true)]
#[case::metadata("169.254.169.254", true)]
#[case::unspecified("0.0.0.0", true)]
#[case::v6_loopback("::1", true)]
#[case::v6_link_local("fe80::1", true)]
#[case::mapped_loopback("::ffff:127.0.0.1", true)]
#[case::private("10.1.2.3", false)]
#[case::public("93.184.216.34", false)]
#[case::v6_ula("fd00::1", false)]
fn guarded____address____loopback_and_link_local_only(#[case] ip: &str, #[case] expected: bool) {
    assert_eq!(guarded(ip.parse().unwrap()), expected);
}
