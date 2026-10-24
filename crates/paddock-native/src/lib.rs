//! Native-process apps for Paddock (ROADMAP N2, spike S19).
//!
//! A native app runs as a process the node spawns and supervises like an
//! instance, talking to the node over its stdin and stdout: an
//! Erlang-style port.
//!
//! - [`protocol`]: the frames both sides exchange.
//! - `app` (feature `app`): a native app as an executable, with the
//!   process functions.
//! - `runner` (feature `runner`, Linux): `paddock-runner`, the launcher
//!   that restricts itself and then loads a rustbridge plugin or executes
//!   the app.

pub mod protocol;

#[cfg(all(unix, feature = "app"))]
pub mod app;

#[cfg(all(target_os = "linux", feature = "runner"))]
pub mod runner;
