//! Logging through `wasi:logging`. Declare the `wasi:logging` capability in
//! the manifest; records carry the app, version and pid, and the host caps
//! their level and rate.
//!
//! ```ignore
//! paddock_sdk::info!("order {} accepted", id);
//! ```
//!
//! The host tells the instance its effective level (the manifest's, or the
//! operator's from `paddock log-level`) with a `log-level` host event, which
//! the SDK handles itself; `App::handle_info` does not see it. From then on
//! the macros skip records below that level before formatting them, so a
//! filtered `debug!` costs a load and a compare. Until the event arrives
//! (during `init`), every record goes to the host, which filters it.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::bindings::wasi::logging::logging;

/// The host event that carries the effective level.
pub(crate) const LEVEL_EVENT: &str = "log-level";

/// The lowest level the host emits, as [`Level::rank`]. Zero lets every
/// record through to the host.
static MIN_LEVEL: AtomicU8 = AtomicU8::new(0);

/// Log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Trace.
    Trace,
    /// Debug.
    Debug,
    /// Info.
    Info,
    /// Warn.
    Warn,
    /// Error.
    Error,
    /// Critical.
    Critical,
}

impl Level {
    fn rank(self) -> u8 {
        match self {
            Self::Trace => 0,
            Self::Debug => 1,
            Self::Info => 2,
            Self::Warn => 3,
            Self::Error => 4,
            Self::Critical => 5,
        }
    }
}

/// Whether the host would emit a record at `level`. The macros check this
/// before formatting.
#[must_use]
pub fn enabled(level: Level) -> bool {
    level.rank() >= MIN_LEVEL.load(Ordering::Relaxed)
}

/// Apply a `log-level` host event. An unknown level lets every record
/// through; the host still filters.
pub(crate) fn set_from_host(body: &[u8]) {
    let rank = match body {
        b"debug" => Level::Debug.rank(),
        b"info" => Level::Info.rank(),
        b"warn" => Level::Warn.rank(),
        b"error" => Level::Error.rank(),
        b"critical" => Level::Critical.rank(),
        _ => Level::Trace.rank(),
    };
    MIN_LEVEL.store(rank, Ordering::Relaxed);
}

/// Write one record with context `app`, unless the host would drop it.
pub fn write(level: Level, message: &str) {
    if !enabled(level) {
        return;
    }
    let level = match level {
        Level::Trace => logging::Level::Trace,
        Level::Debug => logging::Level::Debug,
        Level::Info => logging::Level::Info,
        Level::Warn => logging::Level::Warn,
        Level::Error => logging::Level::Error,
        Level::Critical => logging::Level::Critical,
    };
    logging::log(level, "app", message);
}

/// Log at `level` with `format!` arguments, formatting only if the host
/// would emit the record. The level macros below expand to this.
#[doc(hidden)]
#[macro_export]
macro_rules! __log {
    ($level:expr, $($arg:tt)*) => {
        if $crate::log::enabled($level) {
            $crate::log::write($level, &::std::format!($($arg)*))
        }
    };
}

/// Log at trace level, with `format!` arguments.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::__log!($crate::log::Level::Trace, $($arg)*) };
}

/// Log at debug level, with `format!` arguments.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::__log!($crate::log::Level::Debug, $($arg)*) };
}

/// Log at info level, with `format!` arguments.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::__log!($crate::log::Level::Info, $($arg)*) };
}

/// Log at warn level, with `format!` arguments.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::__log!($crate::log::Level::Warn, $($arg)*) };
}

/// Log at error level, with `format!` arguments.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::__log!($crate::log::Level::Error, $($arg)*) };
}

#[cfg(test)]
#[path = "log_tests.rs"]
mod log_tests;
