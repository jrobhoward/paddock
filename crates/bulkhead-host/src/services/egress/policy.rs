//! Allow entries for HTTP egress: what a manifest's
//! `wasi:http/outgoing-handler.allow` and node config's `[egress] allow`
//! list, and whether a request or another entry falls inside them.
//!
//! An entry names a scheme, a host and a port:
//!
//! | Entry | Allows |
//! | --- | --- |
//! | `https://api.example.com` | that host, HTTPS, port 443 |
//! | `http://plc.internal:8080` | that host, plain HTTP, port 8080 |
//! | `api.example.com` | the same as `https://api.example.com` |
//! | `*.internal` | any host under `internal` (not `internal` itself), HTTPS, port 443 |
//! | `http://10.0.0.5:8080`, `https://[fd00::1]` | that address; never a host name that resolves to it |
//!
//! Host names compare without case. A path, query, user or a bare `*` is
//! refused.

use std::net::IpAddr;

/// A URL scheme egress speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

/// The host part of an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Host {
    /// One host name, lower case.
    Name(String),
    /// Every name under this suffix (`*.internal` is `Under("internal")`).
    Under(String),
    /// One address.
    Ip(IpAddr),
}

/// One allow entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Allow {
    pub(crate) scheme: Scheme,
    pub(crate) host: Host,
    pub(crate) port: u16,
}

/// Parse an allow entry.
pub(crate) fn parse(entry: &str) -> Result<Allow, String> {
    let bad = |why: &str| format!("allow entry {entry:?}: {why}");
    let (scheme, rest) = match entry.split_once("://") {
        Some(("https", rest)) => (Scheme::Https, rest),
        Some(("http", rest)) => (Scheme::Http, rest),
        Some((other, _)) => return Err(bad(&format!("scheme {other} is not http or https"))),
        None => (Scheme::Https, entry),
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.contains(['/', '?', '#', '@']) {
        return Err(bad("only a scheme, host and port, no path, query or user"));
    }
    let (host, port) = split_port(rest).map_err(|e| bad(&e))?;
    let port = port.unwrap_or(scheme.default_port());
    let host = if let Some(ip) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Host::Ip(ip.parse().map_err(|_| bad("not an IPv6 address"))?)
    } else if let Ok(ip) = host.parse::<IpAddr>() {
        Host::Ip(ip)
    } else if let Some(suffix) = host.strip_prefix("*.") {
        check_name(suffix).map_err(|e| bad(&e))?;
        Host::Under(suffix.to_ascii_lowercase())
    } else {
        check_name(host).map_err(|e| bad(&e))?;
        Host::Name(host.to_ascii_lowercase())
    };
    Ok(Allow { scheme, host, port })
}

/// `host[:port]`, with an IPv6 address in brackets.
fn split_port(text: &str) -> Result<(&str, Option<u16>), String> {
    let (host, port) = if text.starts_with('[') {
        match text.find(']') {
            Some(end) => match &text[end + 1..] {
                "" => (text, None),
                p => match p.strip_prefix(':') {
                    Some(p) => (&text[..=end], Some(p)),
                    None => return Err("text after the address".into()),
                },
            },
            None => return Err("unclosed [".into()),
        }
    } else {
        match text.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, Some(p)),
            Some(_) => return Err("an IPv6 address needs brackets".into()),
            None => (text, None),
        }
    };
    let port = match port {
        Some(p) => Some(
            p.parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .ok_or_else(|| format!("port {p:?} is not 1 to 65535"))?,
        ),
        None => None,
    };
    Ok((host, port))
}

/// A DNS name: labels of letters, digits and hyphens.
fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(format!("{name:?} is not a host name"))
    }
}

impl Allow {
    /// Whether a request to `scheme://host:port` is inside this entry. An
    /// address in `host` matches only an address entry.
    // Only the sender (cargo feature `egress`) checks requests.
    #[cfg_attr(not(feature = "egress"), allow(dead_code))]
    pub(crate) fn allows(&self, scheme: Scheme, host: &str, port: u16) -> bool {
        if self.scheme != scheme || self.port != port {
            return false;
        }
        let host = host.trim_end_matches('.');
        let address = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host)
            .parse::<IpAddr>();
        match (&self.host, address) {
            (Host::Ip(ip), Ok(a)) => *ip == a,
            (_, Ok(_)) | (Host::Ip(_), Err(_)) => false,
            (Host::Name(name), Err(_)) => host.eq_ignore_ascii_case(name),
            (Host::Under(suffix), Err(_)) => under(host, suffix),
        }
    }

    /// Whether every request `other` allows, this entry allows too.
    pub(crate) fn covers(&self, other: &Self) -> bool {
        if self.scheme != other.scheme || self.port != other.port {
            return false;
        }
        match (&self.host, &other.host) {
            (Host::Ip(a), Host::Ip(b)) => a == b,
            (Host::Name(a), Host::Name(b)) => a == b,
            (Host::Under(a), Host::Name(b)) => under(b, a),
            (Host::Under(a), Host::Under(b)) => a == b || under(b, a),
            _ => false,
        }
    }
}

/// Whether `host` is a name below `suffix`.
fn under(host: &str, suffix: &str) -> bool {
    host.len() > suffix.len() + 1
        && host[host.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
}

/// Parse a list of entries.
pub(crate) fn parse_all(entries: &[String]) -> Result<Vec<Allow>, String> {
    entries.iter().map(|e| parse(e)).collect()
}

/// Check a manifest's allow entries against node config's ceiling: each
/// must parse and, with a ceiling, fall inside one of its entries.
///
/// # Errors
///
/// What is wrong with the first entry that fails, for the operator.
pub fn within(allow: &[String], ceiling: Option<&[String]>) -> Result<(), String> {
    let wanted = parse_all(allow)?;
    let Some(ceiling) = ceiling else {
        return Ok(());
    };
    let ceiling = parse_all(ceiling).map_err(|e| format!("[egress] {e}"))?;
    for (entry, parsed) in allow.iter().zip(&wanted) {
        if !ceiling.iter().any(|c| c.covers(parsed)) {
            return Err(format!(
                "allow entry {entry:?} is outside the node's [egress] allow"
            ));
        }
    }
    Ok(())
}

/// Whether a name resolving to `ip` is refused without `allow_loopback`:
/// loopback, link-local (cloud metadata at 169.254.169.254) and unspecified
/// addresses, also as IPv4-mapped IPv6.
#[cfg_attr(not(feature = "egress"), allow(dead_code))]
pub(crate) fn guarded(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_link_local() || v4.is_unspecified(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => guarded(IpAddr::V4(v4)),
            None => {
                v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        },
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod policy_tests;
