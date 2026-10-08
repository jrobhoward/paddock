//! The app's configuration, through `wasi:config` (cargo feature `config`).
//!
//! Declare the `wasi:config` capability in the manifest. Values are the
//! manifest's `config.defaults`, overridden by the node's `[config.<app>]`;
//! they are read-only and fixed for the instance's life.
//!
//! ```ignore
//! let host = paddock_sdk::config::get("db.host")?.unwrap_or_default();
//! ```

use std::fmt;

use crate::bindings::wasi::config::store;

/// Why a config read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The operator did not grant `wasi:config`.
    Denied,
    /// The config source failed.
    Upstream(String),
    /// Reading it failed.
    Io(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => f.write_str("access denied"),
            Self::Upstream(e) => write!(f, "config source: {e}"),
            Self::Io(e) => write!(f, "config read: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<store::Error> for ConfigError {
    fn from(e: store::Error) -> Self {
        match e {
            // `wasi:config` has no denial case; Paddock's host reports one as
            // this `io` error.
            store::Error::Io(msg) if msg == "access-denied" => Self::Denied,
            store::Error::Io(msg) => Self::Io(msg),
            store::Error::Upstream(msg) => Self::Upstream(msg),
        }
    }
}

/// The value of `key`, if set.
///
/// # Errors
///
/// [`ConfigError::Denied`] without the capability granted, and the host's
/// errors otherwise.
pub fn get(key: &str) -> Result<Option<String>, ConfigError> {
    Ok(store::get(key)?)
}

/// Every key and value.
///
/// # Errors
///
/// As for [`get`].
pub fn get_all() -> Result<Vec<(String, String)>, ConfigError> {
    Ok(store::get_all()?)
}
