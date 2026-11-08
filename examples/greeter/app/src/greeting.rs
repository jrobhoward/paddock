//! The `example:greeting` client: the package's functions, through the
//! node (`bulkhead_sdk::extension`). The error is the one the extension
//! encodes.

use bulkhead_sdk::extension;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Why a greeting failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Error {
    /// The operator did not grant `example:greeting` to this app.
    AccessDenied,
    /// Anything else.
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("access denied"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

/// `function` of `example:greeting`, with a failure to call it as the
/// interface's error: a node without the extension denies access.
fn call<A: Serialize, T: DeserializeOwned>(function: &str, args: &A) -> Result<T, Error> {
    extension::call("example:greeting", function, args).unwrap_or_else(|e| {
        Err(match e {
            extension::Error::NoSuchExtension => Error::AccessDenied,
            other => Error::Other(other.to_string()),
        })
    })
}

/// Greet `name` with the salutation the manifest configures.
///
/// # Errors
///
/// [`Error::AccessDenied`] if the operator did not grant the package, or
/// the node runs no greeter.
pub fn hello(name: &str) -> Result<String, Error> {
    call("greet#hello", &(name,))
}

/// How many greetings this node has made, every app together.
///
/// # Errors
///
/// As [`hello`].
pub fn count() -> Result<u64, Error> {
    call("greet#count", &())
}
