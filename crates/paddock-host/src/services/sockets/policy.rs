//! Which addresses an app's sockets may use: the manifest's
//! `wasi:sockets` rules, the address guard, and node config's ceiling.
//!
//! | Use | Allowed when |
//! | --- | --- |
//! | bind (TCP, UDP) | the address is unspecified with port 0 (the implicit bind before a connect), or a `bind` rule matches |
//! | listen | `listen` is set and a `bind` rule matches the local address |
//! | accept | `listen` is set |
//! | connect, UDP send | a `connect` rule matches, past the guard |
//! | UDP receive | `listen` is set, or a `connect` rule matches past the guard |
//!
//! The guard: a destination in a loopback, link-local or unspecified
//! block (`127.0.0.0/8`, `::1`, `169.254.0.0/16`, `fe80::/10`,
//! `0.0.0.0/8`, `::`) is reached only through a rule whose block lies
//! inside that one, so `127.0.0.1/32` reaches the node's loopback and
//! `0.0.0.0/0` does not. An IPv4 address written as IPv6
//! (`::ffff:127.0.0.1`) is checked as IPv4.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use paddock_bundle::manifest::{PortRange, SocketRule, SocketsScope, cidr_contains, parse_cidr};
use wasmtime_wasi::sockets::SocketAddrUse;

/// Blocks reached only by a rule inside them.
const GUARDED: &[(IpAddr, u8)] = &[
    (IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)), 8),
    (IpAddr::V4(Ipv4Addr::new(169, 254, 0, 0)), 16),
    (IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8),
    (IpAddr::V6(Ipv6Addr::LOCALHOST), 128),
    (IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0)), 10),
    (IpAddr::V6(Ipv6Addr::UNSPECIFIED), 128),
];

/// One parsed rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rule {
    net: (IpAddr, u8),
    ports: Option<PortRange>,
}

impl Rule {
    fn parse(rule: &SocketRule) -> Option<Self> {
        Some(Self {
            net: parse_cidr(&rule.cidr)?,
            ports: rule.ports,
        })
    }

    fn matches(&self, ip: IpAddr, port: u16) -> bool {
        cidr_contains(self.net, ip) && self.ports.is_none_or(|p| (p.start..=p.end).contains(&port))
    }

    /// Whether every address and port `other` matches, this rule matches.
    fn covers(&self, other: &Self) -> bool {
        net_inside(other.net, self.net)
            && match (self.ports, other.ports) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(outer), Some(inner)) => outer.start <= inner.start && inner.end <= outer.end,
            }
    }
}

/// Whether block `inner` lies inside block `outer`.
fn net_inside(inner: (IpAddr, u8), outer: (IpAddr, u8)) -> bool {
    inner.1 >= outer.1 && cidr_contains(outer, inner.0)
}

/// What one app's sockets may do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Policy {
    bind: Vec<Rule>,
    connect: Vec<Rule>,
    listen: bool,
}

impl Policy {
    /// The policy a manifest's scope describes. Rules that do not parse
    /// are dropped: validation refused them before deploy.
    pub(crate) fn new(scope: &SocketsScope) -> Self {
        Self {
            bind: scope.bind.iter().filter_map(Rule::parse).collect(),
            connect: scope.connect.iter().filter_map(Rule::parse).collect(),
            listen: scope.listen,
        }
    }

    /// Whether the app may resolve host names: only to connect somewhere.
    pub(crate) fn name_lookup(&self) -> bool {
        !self.connect.is_empty()
    }

    /// Whether `addr` may be used for `why`.
    pub(crate) fn allows(&self, addr: SocketAddr, why: SocketAddrUse) -> bool {
        let ip = addr.ip().to_canonical();
        let port = addr.port();
        match why {
            SocketAddrUse::TcpBind | SocketAddrUse::UdpBind => {
                (ip.is_unspecified() && port == 0) || self.bind.iter().any(|r| r.matches(ip, port))
            }
            SocketAddrUse::TcpListen => {
                self.listen && self.bind.iter().any(|r| r.matches(ip, port))
            }
            SocketAddrUse::TcpAccept => self.listen,
            SocketAddrUse::TcpConnect | SocketAddrUse::UdpSend => self.reaches(ip, port),
            SocketAddrUse::UdpReceive => self.listen || self.reaches(ip, port),
        }
    }

    /// Whether a `connect` rule reaches `ip:port`, past the guard.
    fn reaches(&self, ip: IpAddr, port: u16) -> bool {
        let guard = GUARDED.iter().find(|g| cidr_contains(**g, ip));
        self.connect
            .iter()
            .any(|r| r.matches(ip, port) && guard.is_none_or(|g| net_inside(r.net, *g)))
    }
}

/// Check a manifest's scope against node config's `[sockets]` ceiling:
/// each rule must fall inside one of the ceiling's rules of the same list,
/// and `listen` needs the ceiling's `listen`.
///
/// # Errors
///
/// What is wrong with the first rule outside the ceiling, for the operator.
pub fn within(scope: &SocketsScope, ceiling: &SocketsScope) -> Result<(), String> {
    for (list, rules, outer) in [
        ("bind", &scope.bind, &ceiling.bind),
        ("connect", &scope.connect, &ceiling.connect),
    ] {
        let outer: Vec<Rule> = outer.iter().filter_map(Rule::parse).collect();
        for rule in rules {
            let Some(parsed) = Rule::parse(rule) else {
                return Err(format!("{list} rule {:?} is not a CIDR block", rule.cidr));
            };
            if !outer.iter().any(|o| o.covers(&parsed)) {
                let ports = rule
                    .ports
                    .map_or_else(String::new, |p| format!(" ports {p}"));
                return Err(format!(
                    "{list} rule {}{ports} is outside the node's [sockets] {list}",
                    rule.cidr
                ));
            }
        }
    }
    if scope.listen && !ceiling.listen {
        return Err("listen is not allowed by the node's [sockets]".to_string());
    }
    Ok(())
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod policy_tests;
