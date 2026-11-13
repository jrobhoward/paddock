//! Where a native app finds its preopened directories (ROADMAP Z8b).
//!
//! A manifest preopens each directory at a guest path (`/data`, `/priv`).
//! A native app sees the host's filesystem, narrowed by Landlock to those
//! directories, so the node tells it where each one is: [`ENV`] holds
//! every preopen's guest path and host directory, and [`path`] maps a
//! guest path (`/data/orders.db`) to the file on the host, through
//! `bulkhead_sdk::files::path`.
//!
//! The value is one line per preopen, the guest path and the host
//! directory separated by a tab; a backslash, tab or newline inside
//! either is written `\\`, `\t` or `\n`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The environment variable holding the preopens, set by the node.
pub const ENV: &str = "BULKHEAD_PREOPENS";

/// The environment variable holding, on FreeBSD, each preopen's descriptor
/// in [`ENV`]'s order, joined by commas: the runner opens them before
/// capability mode, which refuses opening by path (ROADMAP FB2). `-` marks
/// one the runner could not open (its user may not), under which every
/// open is refused with `PermissionDenied`.
pub const FDS_ENV: &str = "BULKHEAD_PREOPEN_FDS";

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

/// Where `guest` (an absolute guest path) lies: the index of the preopen
/// whose guest path is its longest prefix, and the path below it (empty
/// for the preopen itself); `None` when no preopen covers it, or when it
/// has a `.`, `..` or empty segment.
#[must_use]
pub fn locate<'g>(preopens: &[Preopen], guest: &'g str) -> Option<(usize, &'g str)> {
    let rest = guest.strip_prefix('/')?;
    if guest.contains("//") || rest.split('/').any(|seg| seg == "." || seg == "..") {
        return None;
    }
    preopens
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let below = guest.strip_prefix(p.guest.as_str())?;
            if below.is_empty() {
                return Some((i, p.guest.len(), ""));
            }
            Some((i, p.guest.len(), below.strip_prefix('/')?))
        })
        .max_by_key(|(_, len, _)| *len)
        .map(|(i, _, below)| (i, below))
}

/// The host file for `guest` (an absolute guest path) under the preopen
/// whose guest path is its longest prefix; `None` when no preopen covers
/// it, or when it has a `.`, `..` or empty segment.
#[must_use]
pub fn resolve(preopens: &[Preopen], guest: &str) -> Option<PathBuf> {
    let (i, below) = locate(preopens, guest)?;
    let host = &preopens[i].host;
    // The preopen itself: its own path, which may be a file (a driver's,
    // ROADMAP DA1), not joined with "" into `path/`.
    Some(if below.is_empty() {
        host.clone()
    } else {
        host.join(below)
    })
}

/// This process's preopens, from [`ENV`] (none without it), read once.
#[must_use]
pub fn from_env() -> &'static [Preopen] {
    static PREOPENS: OnceLock<Vec<Preopen>> = OnceLock::new();
    PREOPENS.get_or_init(|| std::env::var(ENV).map(|v| decode(&v)).unwrap_or_default())
}

/// The host file for `guest` in this process ([`resolve`] over
/// [`from_env`]), or `guest` itself when no preopen covers it: the
/// sandbox then refuses it.
#[must_use]
pub fn path(guest: &str) -> PathBuf {
    resolve(from_env(), guest).unwrap_or_else(|| PathBuf::from(guest))
}

/// How [`open`] opens a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mode {
    /// Read only.
    Read,
    /// Write, created if missing, emptied if present.
    Write,
    /// Write at the end, created if missing.
    Append,
}

/// Open the file at `guest`, an absolute guest path at or under a preopen
/// (`/data/orders.json`), on both systems: on FreeBSD, where capability
/// mode refuses opening by path, relative to the preopen's descriptor the
/// runner passed ([`FDS_ENV`]; ROADMAP FB2); elsewhere, or without one, by
/// the host path [`path`] gives, under the runner's Landlock rules.
///
/// # Errors
///
/// No preopen covers `guest` (`NotFound`), or the open failed: the
/// sandbox's refusal included.
#[cfg(all(unix, feature = "app"))]
pub fn open(guest: &str, mode: Mode) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode as Perm, OFlags};
    if let Some((dir, below)) = by_descriptor(guest)? {
        let flags = OFlags::CLOEXEC
            | match mode {
                Mode::Read => OFlags::RDONLY,
                Mode::Write => OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
                Mode::Append => OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND,
            };
        if below.is_empty() {
            // A preopened file: its own descriptor, duplicated.
            return Ok(std::fs::File::from(dir.try_clone_to_owned()?));
        }
        let fd = rustix::fs::openat(dir, below, flags, Perm::from_raw_mode(0o640))?;
        return Ok(std::fs::File::from(fd));
    }
    let host = path(guest);
    match mode {
        Mode::Read => std::fs::File::open(host),
        Mode::Write => std::fs::File::create(host),
        Mode::Append => std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(host),
    }
}

/// Make the directory at `guest`, as [`open`] reaches files.
///
/// # Errors
///
/// As [`open`].
#[cfg(all(unix, feature = "app"))]
pub fn create_dir(guest: &str) -> std::io::Result<()> {
    if let Some((dir, below)) = by_descriptor(guest)? {
        rustix::fs::mkdirat(dir, below, rustix::fs::Mode::from_raw_mode(0o750))?;
        return Ok(());
    }
    std::fs::create_dir(path(guest))
}

/// Remove the file at `guest`, as [`open`] reaches files.
///
/// # Errors
///
/// As [`open`].
#[cfg(all(unix, feature = "app"))]
pub fn remove_file(guest: &str) -> std::io::Result<()> {
    if let Some((dir, below)) = by_descriptor(guest)? {
        rustix::fs::unlinkat(dir, below, rustix::fs::AtFlags::empty())?;
        return Ok(());
    }
    std::fs::remove_file(path(guest))
}

/// The preopen descriptor `guest` lies under and the path below it, when
/// the runner passed descriptors ([`FDS_ENV`]); `None` without them.
#[cfg(all(unix, feature = "app"))]
fn by_descriptor(guest: &str) -> std::io::Result<Option<(std::os::fd::BorrowedFd<'static>, &str)>> {
    static FDS: OnceLock<Vec<Option<std::os::fd::RawFd>>> = OnceLock::new();
    let fds = FDS.get_or_init(|| {
        std::env::var(FDS_ENV)
            .map(|v| v.split(',').map(|f| f.parse().ok()).collect())
            .unwrap_or_default()
    });
    if fds.is_empty() {
        return Ok(None);
    }
    let (i, below) = locate(from_env(), guest).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no preopen covers {guest}"),
        )
    })?;
    let fd = fds
        .get(i)
        .ok_or_else(|| std::io::Error::other(format!("no descriptor for the preopen of {guest}")))?
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("the preopen of {guest} could not be opened"),
            )
        })?;
    // SAFETY: the runner opened this descriptor for the process and left
    // it open for its life; nothing here closes it.
    Ok(Some((
        unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
        below,
    )))
}

#[cfg(test)]
#[path = "preopens_tests.rs"]
mod preopens_tests;
