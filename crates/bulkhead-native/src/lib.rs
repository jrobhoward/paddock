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
//! - `channel` (feature `channel`, Linux): the shared-memory rings they
//!   travel over, and the socket descriptors are passed on.
//! - `app` (feature `app`): a native app as an executable, with the
//!   process functions.
//! - `runner` (feature `runner`, Linux): `bulkhead-runner`, the launcher
//!   that restricts itself and then loads a rustbridge plugin or executes
//!   the app.

pub mod hot;
pub mod preopens;
pub mod protocol;
pub mod snapshot;

#[cfg(all(target_os = "linux", feature = "channel"))]
pub mod channel;

#[cfg(all(target_os = "linux", feature = "channel"))]
pub mod lease;

#[cfg(all(unix, feature = "app"))]
pub mod app;

#[cfg(all(target_os = "linux", feature = "runner"))]
pub mod runner;
