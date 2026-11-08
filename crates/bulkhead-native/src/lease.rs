//! A singleton's lease, enforced beside the app by `bulkhead-runner`.
//!
//! A node kills a native singleton's process when its failover lease
//! ends, but only while the node itself runs: a node process stopped or
//! stalled past its lease cannot, and the app's process, which is not the
//! node's, runs on (the scada soak found it: about 3 s past the lease
//! while the node was stopped). So for an instance bound to a lease the
//! node starts the runner with [`FLAG`] and the read end of a pipe at
//! [`LEASE_FD`]. The runner forks: the child becomes the app, and the
//! parent stays as a watchdog that kills it once the latest deadline the
//! node wrote has passed, with or without the node, then exits with
//! [`KILLED_EXIT`].
//!
//! The node writes each deadline as 8 bytes, little endian: the lease's
//! end less its margin on `CLOCK_MONOTONIC`, in nanoseconds ([`now_ns`]),
//! [`NO_DEADLINE`] while no lease limits it, and 0 when it may not run.
//! It writes the first before the runner starts and a new one whenever
//! the lease changes. A write of 8 bytes on a pipe is atomic, so the
//! watchdog reads whole deadlines and keeps the last. The app never holds
//! the pipe: the child closes it before anything of the app's runs, and
//! the watchdog's memory is out of the app's reach (it is not dumpable,
//! and the app's seccomp filter refuses `ptrace` and the other processes'
//! memory).

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};

/// The runner's flag for an instance bound to a lease.
pub const FLAG: &str = "--lease";

/// The descriptor the runner finds the lease pipe's read end at.
pub const LEASE_FD: RawFd = 7;

/// The runner's exit status after it killed the app at the lease's end.
pub const KILLED_EXIT: u8 = 125;

/// The deadline for a lease that does not limit the app.
pub const NO_DEADLINE: u64 = u64::MAX;

/// `CLOCK_MONOTONIC` now, in nanoseconds: the clock deadlines are on,
/// which the node's `Instant` reads too.
#[must_use]
pub fn now_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(t.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(t.tv_nsec).unwrap_or(0))
}

/// Put the lease pipe's read end `read` at [`LEASE_FD`], in a child
/// between fork and exec, after `channel::install`: `read` must not be
/// one of the channel's descriptors (copy it above them first).
///
/// # Errors
///
/// The `dup2` failed.
pub fn install(read: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: `LEASE_FD` is a descriptor number this child owns from here
    // on; whatever it held is replaced, as `dup2` does, and the `OwnedFd`
    // is forgotten, so nothing closes it before exec.
    let mut target = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(LEASE_FD) };
    let done = rustix::io::dup2(read, &mut target);
    std::mem::forget(target);
    done?;
    Ok(())
}

/// The last whole deadline in `bytes` read from the pipe, if any.
#[must_use]
pub fn last_deadline(bytes: &[u8]) -> Option<u64> {
    let whole = bytes.len() / 8 * 8;
    let last = bytes.get(whole.checked_sub(8)?..whole)?;
    Some(u64::from_le_bytes(last.try_into().ok()?))
}

#[cfg(test)]
#[path = "lease_tests.rs"]
mod lease_tests;
