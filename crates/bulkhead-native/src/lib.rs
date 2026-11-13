//! Native-process apps for Bulkhead (spike S19).
//!
//! A native app runs as a process the node spawns and supervises like an
//! instance, talking to the node over a channel of its own (two rings in
//! shared memory): an Erlang-style port.
//!
//! - [`protocol`]: the frames both sides exchange.
//! - [`hot`]: the most frequent of them in fixed layouts, beside CBOR.
//! - [`snapshot`]: a published bucket's snapshot, which apps map and read.
//! - [`preopens`]: where a native app finds its preopened directories.
//! - `channel` (feature `channel`, Linux and FreeBSD): the shared-memory
//!   rings they travel over, and the socket descriptors are passed on.
//! - `freebsd` (feature `channel`, FreeBSD): system calls rustix lacks.
//! - `app` (feature `app`): a native app as an executable, with the
//!   process functions.
//! - `runner` (feature `runner`, Linux and FreeBSD): `bulkhead-runner`,
//!   the launcher that restricts itself (Landlock and seccomp on Linux,
//!   Capsicum on FreeBSD) and then loads a rustbridge plugin or executes
//!   the app.

pub mod driver;
pub mod hot;
pub mod preopens;
pub mod protocol;
pub mod snapshot;

#[cfg(all(any(target_os = "linux", target_os = "freebsd"), feature = "channel"))]
pub mod channel;

#[cfg(all(any(target_os = "linux", target_os = "freebsd"), feature = "channel"))]
pub mod lease;

#[cfg(all(target_os = "freebsd", feature = "channel"))]
pub mod freebsd;

#[cfg(all(unix, feature = "app"))]
pub mod app;

#[cfg(all(any(target_os = "linux", target_os = "freebsd"), feature = "app"))]
pub mod sender;

#[cfg(all(any(target_os = "linux", target_os = "freebsd"), feature = "runner"))]
pub mod runner;
