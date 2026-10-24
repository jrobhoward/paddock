#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use paddock_bundle::manifest::{SocketRule, SocketsScope};
use rstest::rstest;
use wasmtime_wasi::sockets::SocketAddrUse;

use super::{Policy, within};

fn rule(cidr: &str, ports: Option<&str>) -> SocketRule {
    SocketRule {
        cidr: cidr.to_string(),
        ports: ports.map(|p| p.parse().unwrap()),
    }
}

fn scope(
    bind: &[(&str, Option<&str>)],
    connect: &[(&str, Option<&str>)],
    listen: bool,
) -> SocketsScope {
    SocketsScope {
        bind: bind.iter().map(|(c, p)| rule(c, *p)).collect(),
        connect: connect.iter().map(|(c, p)| rule(c, *p)).collect(),
        listen,
        ..SocketsScope::default()
    }
}

#[rstest]
#[case("10.1.2.3:1883", true)]
#[case("10.1.2.3:1884", false)]
#[case("192.168.0.1:1883", false)]
#[case("[::ffff:10.1.2.3]:1883", true)]
fn allows____connect_rule____matches_block_and_ports(#[case] addr: &str, #[case] allowed: bool) {
    let policy = Policy::new(&scope(&[], &[("10.0.0.0/8", Some("1883"))], false));

    let found = policy.allows(addr.parse().unwrap(), SocketAddrUse::TcpConnect);

    assert_eq!(found, allowed);
}

#[rstest]
#[case("127.0.0.1:5432")]
#[case("169.254.169.254:80")]
#[case("0.0.0.0:80")]
#[case("[::1]:80")]
#[case("[fe80::1]:80")]
#[case("[::ffff:127.0.0.1]:80")]
fn allows____broad_rule____guarded_address_refused(#[case] addr: &str) {
    let policy = Policy::new(&scope(&[], &[("0.0.0.0/0", None), ("::/0", None)], false));

    let found = policy.allows(addr.parse().unwrap(), SocketAddrUse::TcpConnect);

    assert!(!found);
}

#[rstest]
#[case("127.0.0.1/32", "127.0.0.1:5432", true)]
#[case("127.0.0.0/8", "127.0.0.9:5432", true)]
#[case("::1/128", "[::1]:5432", true)]
#[case("169.254.169.254/32", "169.254.169.254:80", true)]
#[case("127.0.0.1/32", "127.0.0.2:5432", false)]
fn allows____rule_inside_guarded_block____reaches_it(
    #[case] cidr: &str,
    #[case] addr: &str,
    #[case] allowed: bool,
) {
    let policy = Policy::new(&scope(&[], &[(cidr, None)], false));

    let found = policy.allows(addr.parse().unwrap(), SocketAddrUse::TcpConnect);

    assert_eq!(found, allowed);
}

#[test]
fn allows____implicit_bind_before_connect____allowed_without_bind_rules() {
    let policy = Policy::new(&scope(&[], &[("10.0.0.0/8", None)], false));

    let v4 = policy.allows("0.0.0.0:0".parse().unwrap(), SocketAddrUse::TcpBind);
    let v6 = policy.allows("[::]:0".parse().unwrap(), SocketAddrUse::UdpBind);
    let fixed = policy.allows("0.0.0.0:8080".parse().unwrap(), SocketAddrUse::TcpBind);

    assert!(v4 && v6);
    assert!(!fixed);
}

#[test]
fn allows____listen____needs_flag_and_bind_rule() {
    let rules = [("0.0.0.0/0", Some("9000-9010"))];
    let on = Policy::new(&scope(&rules, &[], true));
    let off = Policy::new(&scope(&rules, &[], false));
    let addr = "0.0.0.0:9005".parse().unwrap();

    let listens = on.allows(addr, SocketAddrUse::TcpListen);
    let refused = off.allows(addr, SocketAddrUse::TcpListen);
    let outside = on.allows("0.0.0.0:9011".parse().unwrap(), SocketAddrUse::TcpListen);
    let accepts = on.allows(
        "203.0.113.9:40000".parse().unwrap(),
        SocketAddrUse::TcpAccept,
    );

    assert!(listens && accepts);
    assert!(!refused && !outside);
}

#[test]
fn allows____udp_receive____from_connect_peers_or_with_listen() {
    let client = Policy::new(&scope(&[], &[("10.0.0.5/32", Some("161"))], false));
    let server = Policy::new(&scope(&[("0.0.0.0/0", Some("514"))], &[], true));
    let peer = "10.0.0.5:161".parse().unwrap();
    let stranger = "10.0.0.6:161".parse().unwrap();

    assert!(client.allows(peer, SocketAddrUse::UdpReceive));
    assert!(!client.allows(stranger, SocketAddrUse::UdpReceive));
    assert!(server.allows(stranger, SocketAddrUse::UdpReceive));
}

#[test]
fn name_lookup____with_and_without_connect_rules____only_with_them() {
    let none = Policy::new(&scope(&[("0.0.0.0/0", None)], &[], true));
    let some = Policy::new(&scope(&[], &[("10.0.0.0/8", None)], false));

    assert!(!none.name_lookup());
    assert!(some.name_lookup());
}

#[rstest]
#[case(&[("10.1.0.0/16", Some("1883"))], true)]
#[case(&[("10.0.0.0/8", None)], false)]
#[case(&[("10.1.0.0/16", Some("1883-1884"))], false)]
#[case(&[("192.168.1.0/24", Some("1883"))], false)]
fn within____connect_rules____inside_ceiling_or_refused(
    #[case] connect: &[(&str, Option<&str>)],
    #[case] ok: bool,
) {
    let ceiling = scope(&[], &[("10.0.0.0/8", Some("1883"))], false);

    let found = within(&scope(&[], connect, false), &ceiling);

    assert_eq!(found.is_ok(), ok, "{found:?}");
}

#[test]
fn within____listen_without_ceiling_listen____refused() {
    let ceiling = scope(&[("0.0.0.0/0", None)], &[], false);

    let found = within(&scope(&[("0.0.0.0/0", Some("9000"))], &[], true), &ceiling);

    assert!(found.unwrap_err().contains("listen"));
}
