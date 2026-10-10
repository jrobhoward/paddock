//! Secrets the operator binds on the node, through `paddock:secrets`
//! (cargo feature `secrets`).
//!
//! List the names in the manifest; the operator grants each and binds it
//! to a file or an environment variable on the node. Read a secret when it
//! is needed rather than keeping it in state: a rotated file is seen on the
//! next read, and state ends up in snapshots.
//!
//! ```ignore
//! let password = paddock_sdk::secrets::get_string("db-password")?;
//! ```

use std::fmt;

use crate::bindings::paddock::secrets::store;

/// Why a secret could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecretError {
    /// Not granted, or not listed in the manifest.
    AccessDenied,
    /// The node binds no source for the name.
    NotFound,
    /// The source could not be read.
    Unavailable(String),
    /// The value is not UTF-8 ([`get_string`] only).
    NotText,
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("access denied"),
            Self::NotFound => f.write_str("not found"),
            Self::Unavailable(why) => write!(f, "unavailable: {why}"),
            Self::NotText => f.write_str("not UTF-8"),
        }
    }
}

impl std::error::Error for SecretError {}

impl From<store::Error> for SecretError {
    fn from(e: store::Error) -> Self {
        match e {
            store::Error::AccessDenied => Self::AccessDenied,
            store::Error::NotFound => Self::NotFound,
            store::Error::Unavailable(why) => Self::Unavailable(why),
        }
    }
}

/// The secret `name`'s bytes.
///
/// # Errors
///
/// As [`SecretError`] lists.
pub fn get(name: &str) -> Result<Vec<u8>, SecretError> {
    Ok(store::get(name)?)
}

/// The secret `name` as text, with one trailing newline removed (files
/// written by `echo` have one).
///
/// # Errors
///
/// As for [`get`], and [`SecretError::NotText`].
pub fn get_string(name: &str) -> Result<String, SecretError> {
    let mut s = String::from_utf8(get(name)?).map_err(|_| SecretError::NotText)?;
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    Ok(s)
}
