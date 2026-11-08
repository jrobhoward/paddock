//! The app's configuration, through `bulkhead:config` (cargo feature `config`).
//!
//! Declare the `bulkhead:config` capability in the manifest. Values are the
//! manifest's `config.defaults`, overridden by the node's `[config.<app>]`;
//! they are read-only and fixed for the instance's life.
//!
//! ```ignore
//! let host = bulkhead_sdk::config::get("db.host")?.unwrap_or_default();
//! ```

use std::fmt;

use bulkhead_native::app::Ctx;
use bulkhead_native::protocol as p;

/// Why a config read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The operator did not grant `bulkhead:config`.
    Denied,
    /// Reading it failed.
    Io(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => f.write_str("access denied"),
            Self::Io(e) => write!(f, "config read: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<p::ConfigError> for ConfigError {
    fn from(e: p::ConfigError) -> Self {
        match e {
            p::ConfigError::AccessDenied => Self::Denied,
            p::ConfigError::Other(why) => Self::Io(why),
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
    Ok(Ctx::current().config(key)?)
}

/// Every key and value.
///
/// # Errors
///
/// As for [`get`].
pub fn get_all() -> Result<Vec<(String, String)>, ConfigError> {
    Ok(Ctx::current().config_all()?)
}
