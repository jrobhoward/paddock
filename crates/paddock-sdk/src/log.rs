//! Logging through `wasi:logging`. Declare the `wasi:logging` capability in
//! the manifest; records carry the app, version and pid, and the host caps
//! their level and rate.
//!
//! ```ignore
//! paddock_sdk::info!("order {} accepted", id);
//! ```

use crate::bindings::wasi::logging::logging;

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

/// Write one record with context `app`.
pub fn write(level: Level, message: &str) {
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

/// Log at debug level, with `format!` arguments.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Debug, &::std::format!($($arg)*)) };
}

/// Log at info level, with `format!` arguments.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Info, &::std::format!($($arg)*)) };
}

/// Log at warn level, with `format!` arguments.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Warn, &::std::format!($($arg)*)) };
}

/// Log at error level, with `format!` arguments.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::write($crate::log::Level::Error, &::std::format!($($arg)*)) };
}
