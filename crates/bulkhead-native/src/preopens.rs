//! Where a native app finds its preopened directories (ROADMAP Z8b).
//!
//! A component sees each directory its manifest preopens at the guest
//! path (`/data`, `/priv`). A native app sees the host's filesystem,
//! narrowed by Landlock to those directories, so the node tells it where
//! each one is: [`ENV`] holds every preopen's guest path and host
//! directory, and [`path`] maps a guest path (`/data/orders.db`) to the
//! file on the host. The same source then builds both ways, through
//! `bulkhead_sdk::files::path`.
//!
//! The value is one line per preopen, the guest path and the host
//! directory separated by a tab; a backslash, tab or newline inside
//! either is written `\\`, `\t` or `\n`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The environment variable holding the preopens, set by the node.
pub const ENV: &str = "BULKHEAD_PREOPENS";

/// One preopened directory: where the app's code names it, and where it
/// is on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preopen {
    /// The guest path, as in the manifest (`/data`).
    pub guest: String,
    /// The host directory.
    pub host: PathBuf,
}

/// [`ENV`]'s value for `preopens`; `None` if a host directory's path is
/// not UTF-8.
#[must_use]
pub fn encode<'a>(preopens: impl IntoIterator<Item = (&'a str, &'a Path)>) -> Option<String> {
    let mut out = String::new();
    for (guest, host) in preopens {
        if !out.is_empty() {
            out.push('\n');
        }
        escape(guest, &mut out);
        out.push('\t');
        escape(host.to_str()?, &mut out);
    }
    Some(out)
}

fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// The preopens in [`ENV`]'s `value`; a malformed line is skipped.
#[must_use]
pub fn decode(value: &str) -> Vec<Preopen> {
    value
        .split('\n')
        .filter_map(|line| {
            let (guest, host) = line.split_once('\t')?;
            Some(Preopen {
                guest: unescape(guest)?,
                host: PathBuf::from(unescape(host)?),
            })
        })
        .collect()
}

fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(match chars.next()? {
                '\\' => '\\',
                't' => '\t',
                'n' => '\n',
                _ => return None,
            });
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// The host file for `guest` (an absolute guest path) under the preopen
/// whose guest path is its longest prefix; `None` when no preopen covers
/// it, or when it has a `.`, `..` or empty segment.
#[must_use]
pub fn resolve(preopens: &[Preopen], guest: &str) -> Option<PathBuf> {
    let rest = guest.strip_prefix('/')?;
    if guest.contains("//") || rest.split('/').any(|seg| seg == "." || seg == "..") {
        return None;
    }
    preopens
        .iter()
        .filter_map(|p| {
            let below = guest.strip_prefix(p.guest.as_str())?;
            let below = if below.is_empty() {
                ""
            } else {
                below.strip_prefix('/')?
            };
            Some((p.guest.len(), p.host.join(below)))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, host)| host)
}

/// This process's preopens, from [`ENV`] (none without it), read once.
#[must_use]
pub fn from_env() -> &'static [Preopen] {
    static PREOPENS: OnceLock<Vec<Preopen>> = OnceLock::new();
    PREOPENS.get_or_init(|| std::env::var(ENV).map(|v| decode(&v)).unwrap_or_default())
}

/// The host file for `guest` in this process ([`resolve`] over
/// [`from_env`]), or `guest` itself when no preopen covers it: the
/// sandbox then refuses it, as WASI refuses a path outside every
/// preopen.
#[must_use]
pub fn path(guest: &str) -> PathBuf {
    resolve(from_env(), guest).unwrap_or_else(|| PathBuf::from(guest))
}

#[cfg(test)]
#[path = "preopens_tests.rs"]
mod preopens_tests;
