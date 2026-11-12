//! What differs between the systems native apps run on (Linux and
//! FreeBSD): one module per system, each with the same items, so callers
//! name `native::os::...` and never a system (ROADMAP FB1).
//!
//! - [`process_bytes`]: the memory an instance's processes hold, for `ps`
//!   and metrics when it has no cgroup.
//! - [`TcpQuiet`]: how long a TCP connection has carried no data, for a
//!   native listener's idle timeout.
//!
//! The rest is the same code on both, through rustix: the rings and
//! published snapshots in sealed memfds (`memfd_create` makes an anonymous
//! shared memory object on FreeBSD, with the same seals), the
//! parent-death signal (`prctl` or `procctl`), `RLIMIT_DATA`. What stays
//! Linux's alone: cgroups (`native::cgroup`; FreeBSD's `rctl` needs root,
//! so it comes with the helper, ROADMAP PS1 and FB2), the seccomp filter's
//! refused calls (`native::syscalls`), and the runner's sandbox.

#[cfg(target_os = "freebsd")]
mod freebsd;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "freebsd")]
pub(crate) use freebsd::{TcpQuiet, process_bytes};
#[cfg(target_os = "linux")]
pub(crate) use linux::{TcpQuiet, process_bytes};

#[cfg(test)]
#[path = "os_tests.rs"]
mod os_tests;
