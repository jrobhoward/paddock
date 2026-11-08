//! The `bulkhead-modbus:modbus` extension's interfaces for apps, in both
//! builds: a component imports them through these bindings; a native
//! program calls the same functions through the node
//! (`bulkhead_sdk::extension`, ROADMAP Z8h), which the extension serves
//! with `Extension::call`. The functions and types are the generated
//! bindings' (`wit/modbus.wit` in `bulkhead-modbus`), so app code is the
//! same either way:
//!
//! ```ignore
//! use bulkhead_modbus_client::client;
//!
//! client::write_holding("pump-1", 10, &[42])?;
//! ```
//!
//! The app declares `capabilities.extensions."bulkhead-modbus:modbus"`
//! with the devices it reads and writes, and the operator grants it.
//! Natively, a node without the extension answers `access-denied`, as its
//! deny stubs answer a component; a call the extension cannot take (a
//! newer client than the node's extension) is `invalid` for the client
//! interface and `not-serving` for the server's.

#[allow(missing_docs, clippy::too_many_arguments)]
mod bindings {
    wit_bindgen::generate!({
        path: "../bulkhead-modbus/wit",
        world: "bulkhead-modbus:modbus/modbus",
        additional_derives: [PartialEq, Eq, serde::Serialize, serde::Deserialize],
    });
}

/// The package apps declare and operators grant.
pub const PACKAGE: &str = "bulkhead-modbus:modbus";

/// Reads and writes on the devices the node polls (`client` in the WIT).
pub mod client {
    pub use crate::bindings::bulkhead_modbus::modbus::client::Error;
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    pub use crate::bindings::bulkhead_modbus::modbus::client::{
        read_coils, read_discrete, read_holding, read_input, write_coils, write_holding,
    };

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    pub use native::*;

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    mod native {
        use super::Error;
        use crate::native::call;

        fn failed(e: bulkhead_sdk::extension::Error) -> Error {
            match e {
                bulkhead_sdk::extension::Error::NoSuchExtension => Error::AccessDenied,
                other => Error::Invalid(other.to_string()),
            }
        }

        /// Read holding registers.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn read_holding(device: &str, address: u16, count: u16) -> Result<Vec<u16>, Error> {
            call("client#read-holding", &(device, address, count), failed)
        }

        /// Read input registers.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn read_input(device: &str, address: u16, count: u16) -> Result<Vec<u16>, Error> {
            call("client#read-input", &(device, address, count), failed)
        }

        /// Read coils.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn read_coils(device: &str, address: u16, count: u16) -> Result<Vec<bool>, Error> {
            call("client#read-coils", &(device, address, count), failed)
        }

        /// Read discrete inputs.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn read_discrete(device: &str, address: u16, count: u16) -> Result<Vec<bool>, Error> {
            call("client#read-discrete", &(device, address, count), failed)
        }

        /// Write holding registers.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn write_holding(device: &str, address: u16, values: &[u16]) -> Result<(), Error> {
            call("client#write-holding", &(device, address, values), failed)
        }

        /// Write coils.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn write_coils(device: &str, address: u16, values: &[bool]) -> Result<(), Error> {
            call("client#write-coils", &(device, address, values), failed)
        }
    }
}

/// The registers the node serves to Modbus masters (`server` in the WIT).
pub mod server {
    pub use crate::bindings::bulkhead_modbus::modbus::server::Error;
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    pub use crate::bindings::bulkhead_modbus::modbus::server::{
        get_coils, get_holding, set_coils, set_discrete, set_holding, set_input,
    };

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    pub use native::*;

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    mod native {
        use super::Error;
        use crate::native::call;

        fn failed(e: bulkhead_sdk::extension::Error) -> Error {
            match e {
                bulkhead_sdk::extension::Error::NoSuchExtension => Error::AccessDenied,
                _ => Error::NotServing,
            }
        }

        /// Set served holding registers.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn set_holding(address: u16, values: &[u16]) -> Result<(), Error> {
            call("server#set-holding", &(address, values), failed)
        }

        /// Set served input registers.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn set_input(address: u16, values: &[u16]) -> Result<(), Error> {
            call("server#set-input", &(address, values), failed)
        }

        /// Set served coils.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn set_coils(address: u16, values: &[bool]) -> Result<(), Error> {
            call("server#set-coils", &(address, values), failed)
        }

        /// Set served discrete inputs.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn set_discrete(address: u16, values: &[bool]) -> Result<(), Error> {
            call("server#set-discrete", &(address, values), failed)
        }

        /// Served holding registers.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn get_holding(address: u16, count: u16) -> Result<Vec<u16>, Error> {
            call("server#get-holding", &(address, count), failed)
        }

        /// Served coils.
        ///
        /// # Errors
        ///
        /// As the WIT's `error`.
        pub fn get_coils(address: u16, count: u16) -> Result<Vec<bool>, Error> {
            call("server#get-coils", &(address, count), failed)
        }
    }
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
mod native {
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    /// `function` of the extension, its failure to be called turned into
    /// the interface's error by `failed`.
    pub(crate) fn call<A: Serialize, T: DeserializeOwned, E: DeserializeOwned>(
        function: &str,
        args: &A,
        failed: fn(bulkhead_sdk::extension::Error) -> E,
    ) -> Result<T, E> {
        bulkhead_sdk::extension::call(crate::PACKAGE, function, args)
            .unwrap_or_else(|e| Err(failed(e)))
    }
}
