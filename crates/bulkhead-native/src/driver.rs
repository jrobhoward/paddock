//! What a driver app gets from its node (ROADMAP DA1): the listening
//! sockets node config `[drivers.<app>]` binds for it, inherited at
//! descriptors from [`FIRST_LISTENER_FD`] and named in [`LISTENERS_ENV`],
//! and its read-only files under [`FILES_PATH`].
//!
//! The node keeps each listening socket for its own life, so a driver's
//! restart or upgrade keeps the port. The driver accepts, reads and writes
//! its connections itself; its seccomp filter allows `accept4` on these
//! descriptors alone.

use std::os::fd::RawFd;

/// The environment variable naming a driver's listening sockets:
/// `name=fd` pairs joined by commas (`https=8,http=9`).
pub const LISTENERS_ENV: &str = "BULKHEAD_LISTENERS";

/// The descriptor a driver's first listening socket is inherited at; the
/// rest follow.
pub const FIRST_LISTENER_FD: RawFd = 8;

/// The most listening sockets one driver gets (descriptors 8 to 13).
pub const MAX_LISTENERS: usize = 6;

/// Where a driver sees the files node config gives it, each by its file
/// name (`/driver/shop.pem`), through `BULKHEAD_PREOPENS`.
pub const FILES_PATH: &str = "/driver";

/// [`LISTENERS_ENV`]'s value for `names`, in order from
/// [`FIRST_LISTENER_FD`].
#[must_use]
pub fn encode<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    names
        .into_iter()
        .zip(FIRST_LISTENER_FD..)
        .map(|(name, fd)| format!("{name}={fd}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The listeners [`LISTENERS_ENV`]'s `value` names, by name and
/// descriptor; malformed pairs are skipped.
#[must_use]
pub fn parse(value: &str) -> Vec<(String, RawFd)> {
    value
        .split(',')
        .filter_map(|pair| {
            let (name, fd) = pair.split_once('=')?;
            let fd: RawFd = fd.parse().ok()?;
            (!name.is_empty()
                && (FIRST_LISTENER_FD..FIRST_LISTENER_FD + MAX_LISTENERS as RawFd).contains(&fd))
            .then(|| (name.to_string(), fd))
        })
        .collect()
}

/// This process's listening sockets, by name, taken once: a later call
/// returns none. Empty for an app that is not a driver.
#[cfg(unix)]
#[must_use]
pub fn listeners() -> Vec<(String, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};

    static TAKEN: AtomicBool = AtomicBool::new(false);
    if TAKEN.swap(true, Ordering::SeqCst) {
        return Vec::new();
    }
    let Ok(value) = std::env::var(LISTENERS_ENV) else {
        return Vec::new();
    };
    parse(&value)
        .into_iter()
        .map(|(name, fd)| {
            // SAFETY: the node put a listening socket at each descriptor
            // the variable names, for this process; `TAKEN` makes this
            // the only owner.
            (name, unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
        })
        .collect()
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod driver_tests;
