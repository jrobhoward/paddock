//! The `example:greeting` client, for both builds: a component imports the
//! package through its generated bindings; a native program calls the same
//! functions through the node (`bulkhead_sdk::extension`). The bindings'
//! types derive serde's traits on both targets, as the extension's do, so
//! the error is one definition for both.

#[allow(missing_docs)]
mod bindings {
    wit_bindgen::generate!({
        path: "../wit",
        world: "example:greeting/client",
        additional_derives: [serde::Serialize, serde::Deserialize],
    });
}

pub use bindings::example::greeting::greet::Error;
#[cfg(not(all(unix, not(target_arch = "wasm32"))))]
pub use bindings::example::greeting::greet::{count, hello};

/// Greet `name` with the salutation the manifest configures.
///
/// # Errors
///
/// [`Error::AccessDenied`] if the operator did not grant the package, or
/// the node runs no greeter.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub fn hello(name: &str) -> Result<String, Error> {
    native::call("greet#hello", &(name,))
}

/// How many greetings this node has made, every app together.
///
/// # Errors
///
/// As [`hello`].
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub fn count() -> Result<u64, Error> {
    native::call("greet#count", &())
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
mod native {
    use bulkhead_sdk::extension;
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    use super::Error;

    /// `function` of `example:greeting`, with a failure to call it as the
    /// interface's error: a node without the extension denies access, as
    /// its deny stubs do for a component.
    pub(super) fn call<A: Serialize, T: DeserializeOwned>(
        function: &str,
        args: &A,
    ) -> Result<T, Error> {
        extension::call("example:greeting", function, args).unwrap_or_else(|e| {
            Err(match e {
                extension::Error::NoSuchExtension => Error::AccessDenied,
                other => Error::Other(other.to_string()),
            })
        })
    }
}
