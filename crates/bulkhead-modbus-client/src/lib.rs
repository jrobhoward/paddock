//! Apps' client for the `bulkhead-modbus:modbus` extension: its functions,
//! through the node (`bulkhead_sdk::extension`), which the extension serves
//! with `Extension::call`.
//!
//! ```ignore
//! use bulkhead_modbus_client::client;
//!
//! client::write_holding("pump-1", 10, &[42])?;
//! ```
//!
//! The app declares `capabilities.extensions."bulkhead-modbus:modbus"`
//! with the devices it reads and writes, and the operator grants it.
//! A node without the extension answers `AccessDenied`; a call the
//! extension cannot take (a newer client than the node's extension) is
//! `Invalid` for the client interface and `NotServing` for the server's.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The package apps declare and operators grant.
pub const PACKAGE: &str = "bulkhead-modbus:modbus";

/// Reads and writes on the devices the node polls.
pub mod client {
    use crate::call;

    /// Why a device request failed.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum Error {
        /// `bulkhead-modbus:modbus` is not granted, or the manifest does
        /// not list the device for this use.
        AccessDenied,
        /// The node does not configure this device.
        NoSuchDevice,
        /// The device cannot be reached (not connected, timed out, closed).
        Offline(String),
        /// The device answered with a Modbus exception code.
        Exception(u8),
        /// The request is invalid (no values, or more than one request
        /// carries: 125 registers or 2000 coils to read, 123 or 1968 to
        /// write).
        Invalid(String),
    }

    crate::displayed!(Error);

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
    /// As [`Error`] lists.
    pub fn read_holding(device: &str, address: u16, count: u16) -> Result<Vec<u16>, Error> {
        call("client#read-holding", &(device, address, count), failed)
    }

    /// Read input registers.
    ///
    /// # Errors
    ///
    /// As [`Error`] lists.
    pub fn read_input(device: &str, address: u16, count: u16) -> Result<Vec<u16>, Error> {
        call("client#read-input", &(device, address, count), failed)
    }

    /// Read coils.
    ///
    /// # Errors
    ///
    /// As [`Error`] lists.
    pub fn read_coils(device: &str, address: u16, count: u16) -> Result<Vec<bool>, Error> {
        call("client#read-coils", &(device, address, count), failed)
    }

    /// Read discrete inputs.
    ///
    /// # Errors
    ///
    /// As [`Error`] lists.
    pub fn read_discrete(device: &str, address: u16, count: u16) -> Result<Vec<bool>, Error> {
        call("client#read-discrete", &(device, address, count), failed)
    }

    /// Write holding registers.
    ///
    /// # Errors
    ///
    /// As [`Error`] lists.
    pub fn write_holding(device: &str, address: u16, values: &[u16]) -> Result<(), Error> {
        call("client#write-holding", &(device, address, values), failed)
    }

    /// Write coils.
    ///
    /// # Errors
    ///
    /// As [`Error`] lists.
    pub fn write_coils(device: &str, address: u16, values: &[bool]) -> Result<(), Error> {
        call("client#write-coils", &(device, address, values), failed)
    }
}

/// The registers the node serves to Modbus masters: masters read them,
/// and their writes change them and are cast to the server's `deliver_to`
/// app.
pub mod server {
    use crate::call;

    /// Why a request failed.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum Error {
        /// The capability is not granted, or the manifest does not set
        /// `serve`.
        AccessDenied,
        /// The node runs no Modbus server.
        NotServing,
        /// The range runs past the table the node configures.
        OutOfRange,
    }

    crate::displayed!(Error);

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
    /// As [`Error`] lists.
    pub fn set_holding(address: u16, values: &[u16]) -> Result<(), Error> {
        call("server#set-holding", &(address, values), failed)
    }

    /// Set served input registers.
    ///
    /// # Errors
    ///
    /// As [`Error`] lists.
    pub fn set_input(address: u16, values: &[u16]) -> Result<(), Error> {
        call("server#set-input", &(address, values), failed)
    }

    /// Set served coils.
    ///
    /// # Errors
    ///
    /// As [`Error`] lists.
    pub fn set_coils(address: u16, values: &[bool]) -> Result<(), Error> {
        call("server#set-coils", &(address, values), failed)
    }

    /// Set served discrete inputs.
    ///
    /// # Errors
    ///
    /// As [`Error`] lists.
    pub fn set_discrete(address: u16, values: &[bool]) -> Result<(), Error> {
        call("server#set-discrete", &(address, values), failed)
    }

    /// Served holding registers.
    ///
    /// # Errors
    ///
    /// As [`Error`] lists.
    pub fn get_holding(address: u16, count: u16) -> Result<Vec<u16>, Error> {
        call("server#get-holding", &(address, count), failed)
    }

    /// Served coils.
    ///
    /// # Errors
    ///
    /// As [`Error`] lists.
    pub fn get_coils(address: u16, count: u16) -> Result<Vec<bool>, Error> {
        call("server#get-coils", &(address, count), failed)
    }
}

/// Display and `Error` for a module's error type.
macro_rules! displayed {
    ($ty:ty) => {
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{self:?}")
            }
        }

        impl std::error::Error for $ty {}
    };
}
pub(crate) use displayed;

/// `function` of the extension, its failure to be called turned into the
/// interface's error by `failed`.
fn call<A: Serialize, T: DeserializeOwned, E: DeserializeOwned>(
    function: &str,
    args: &A,
    failed: fn(bulkhead_sdk::extension::Error) -> E,
) -> Result<T, E> {
    bulkhead_sdk::extension::call(PACKAGE, function, args).unwrap_or_else(|e| Err(failed(e)))
}
