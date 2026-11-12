//! The checks on what the node asks, against the helper's configuration:
//! pure functions, so each is tested without root. A request that fails
//! one is refused before the helper touches anything.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use bulkhead_priv_wire::{MAX_FD_TARGET, RUNNER_ENV, Spawn, is_app_name};

use crate::config::Config;

/// The least memory a spawn may ask: below it the runner cannot start.
pub const MIN_MEMORY: u64 = 4 << 20;

/// `v` as a path if it is absolute and every part after the root is a
/// plain name: no `.`, `..` or empty part (`//`).
#[must_use]
pub fn normal_absolute(v: &str) -> Option<PathBuf> {
    if v == "/" {
        return Some(PathBuf::from(v));
    }
    // By segment, not `Path::components`, which drops a `.` silently.
    let rest = v.strip_prefix('/')?;
    rest.split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
        .then(|| PathBuf::from(v))
}

/// Whether `path` is one of `roots` or under one, part by part.
#[must_use]
pub fn under(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

/// What the helper's own root binds already show, read-only: a bind
/// inside one would be read-only and could not be made.
pub const SYSTEM_DIRS: [&str; 7] = [
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/libx32",
];

/// The kernel's own file systems, which an instance's root makes afresh
/// (`/dev`, `/proc`) or does not have (`/sys`).
pub const KERNEL_DIRS: [&str; 3] = ["/dev", "/proc", "/sys"];

/// Check a spawn against `config`, with `passed` descriptors beside it.
///
/// # Errors
///
/// The first thing wrong with it, as the refusal's reason.
pub fn spawn(config: &Config, s: &Spawn, passed: usize) -> Result<(), String> {
    if !is_app_name(&s.app) {
        return Err(format!("{:?} is not an app name", s.app));
    }
    if s.memory_max < MIN_MEMORY || s.memory_max > config.memory_max {
        return Err(format!(
            "memory_max {} is outside {MIN_MEMORY}..={}",
            s.memory_max, config.memory_max
        ));
    }
    if s.pids_max == 0 || s.pids_max > config.pids_max {
        return Err(format!(
            "pids_max {} is outside 1..={}",
            s.pids_max, config.pids_max
        ));
    }
    for (name, _) in &s.env {
        if !RUNNER_ENV.contains(&name.as_str()) {
            return Err(format!("{name} is not a variable the runner reads"));
        }
    }
    fds(&s.fds, passed)?;
    binds(config, s)?;
    Ok(())
}

/// Each passed descriptor has a target, each target is stdout, stderr or
/// 3 to [`MAX_FD_TARGET`], and no two share one.
///
/// # Errors
///
/// Which rule a target breaks.
pub fn fds(targets: &[u8], passed: usize) -> Result<(), String> {
    if targets.len() != passed {
        return Err(format!(
            "{} descriptor targets for {passed} descriptors",
            targets.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for &t in targets {
        if t == 0 || t > MAX_FD_TARGET {
            return Err(format!("descriptor {t} is outside 1..={MAX_FD_TARGET}"));
        }
        if !seen.insert(t) {
            return Err(format!("descriptor {t} given twice"));
        }
    }
    Ok(())
}

/// Each bind is an absolute path in normal form, under the helper's bind
/// roots, outside the system directories its root shows anyway, and
/// neither inside another nor holding one.
///
/// # Errors
///
/// Which rule a bind breaks.
pub fn binds(config: &Config, s: &Spawn) -> Result<(), String> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for b in &s.binds {
        let path = normal_absolute(&b.path)
            .ok_or_else(|| format!("bind {} is not an absolute path in normal form", b.path))?;
        if !under(&path, &config.binds) {
            return Err(format!("bind {} is outside the helper's binds", b.path));
        }
        if SYSTEM_DIRS
            .iter()
            .chain(&KERNEL_DIRS)
            .any(|d| path.starts_with(d))
        {
            return Err(format!("bind {} is inside a system directory", b.path));
        }
        if let Some(other) = paths
            .iter()
            .find(|p| p.starts_with(&path) || path.starts_with(p))
        {
            return Err(format!(
                "binds {} and {} are one inside the other",
                b.path,
                other.display()
            ));
        }
        paths.push(path);
    }
    Ok(())
}

/// Whether the node may send `signal` to an instance: to stop it, end it
/// or let it go on.
#[must_use]
pub fn signal(signal: i32) -> bool {
    [libc::SIGTERM, libc::SIGKILL, libc::SIGCONT].contains(&signal)
}

/// The address the node asks a listener on, if its port is one the
/// helper binds.
///
/// # Errors
///
/// Not an address, or a port outside the helper's `ports`.
pub fn bind_address(config: &Config, address: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = address
        .parse()
        .map_err(|_| format!("{address} is not an address and port"))?;
    if !config.ports.contains(&addr.port()) {
        return Err(format!("port {} is not one the helper binds", addr.port()));
    }
    Ok(addr)
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod check_tests;
