//! Connection handlers (cargo feature `connection`): one instance per
//! connection accepted on the app's listeners (`paddock:connection`).
//!
//! The manifest declares the listeners, and the node binds each to an
//! address:
//!
//! ```toml
//! [[appserver.listeners]]
//! name = "lines"
//! idle_timeout_ms = 60000
//! ```
//!
//! A handler implements [`Connection`] and reads and writes a [`Stream`],
//! which implements `std::io::Read` and `std::io::Write`. It can call and
//! cast to app instances through [`Ctx`] (finding them with
//! [`Ctx::whereis`]), which is how a connection hands work to an app.
//!
//! ```ignore
//! use std::io::{BufRead, BufReader, Write};
//! use paddock_sdk::connection::{Connection, ConnectionInfo, Stream};
//! use paddock_sdk::Ctx;
//!
//! struct Echo;
//!
//! impl Connection for Echo {
//!     fn serve(_ctx: &mut Ctx, _info: ConnectionInfo, stream: Stream) -> Result<(), String> {
//!         let mut out = stream.clone_writer();
//!         for line in BufReader::new(stream).lines() {
//!             let line = line.map_err(|e| e.to_string())?;
//!             writeln!(out, "{line}").map_err(|e| e.to_string())?;
//!         }
//!         Ok(())
//!     }
//! }
//!
//! paddock_sdk::connection!(Echo);
//! ```
//!
//! Returning closes the connection; an error closes it as a crash. A
//! connection with no bytes in either direction for the listener's idle
//! timeout is closed by the host. A handler has a pid but no mailbox:
//! messages sent to it are dropped.

use std::io;
use std::rc::Rc;

use crate::Ctx;

#[doc(hidden)]
#[allow(missing_docs, clippy::too_many_arguments)]
pub mod bindings {
    wit_bindgen::generate!({
        path: [
            "../../wit",
            "../../wit/vendor/wasi-logging",
            "../../wit/vendor/wasi-config",
            "../../wit/vendor/wasi-keyvalue",
            "../../wit/vendor/wasi-io",
            "../../wit/connection",
            "../../wit/groups",
            "../../wit/watch",
            "../../wit/secrets",
            "../../wit/metrics",
            "wit",
        ],
        world: "paddock:sdk/connection",
        pub_export_macro: true,
        export_macro_name: "export_connection",
        default_bindings_module: "paddock_sdk::connection::bindings",
        with: {
            "paddock:runtime/types@0.1.0": crate::bindings::paddock::runtime::types,
            "paddock:runtime/process@0.1.0": crate::bindings::paddock::runtime::process,
        },
        generate_all,
    });
}

use bindings::exports::paddock::connection::handler as wit;
use bindings::wasi::io::streams::{InputStream, OutputStream, StreamError};

/// Bytes `wasi:io` writes per blocking call.
const WRITE_CHUNK: usize = 4096;

/// What the host knows about a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// The listener's name in the manifest.
    pub listener: String,
    /// The peer's address, `ip:port`.
    pub peer: String,
}

/// A connection handler. The host creates an instance per connection and
/// calls [`Connection::serve`] once.
pub trait Connection {
    /// Serve the connection until done; returning closes it.
    ///
    /// # Errors
    ///
    /// A message closes the connection as a crash, with the message as the
    /// reason.
    fn serve(ctx: &mut Ctx, info: ConnectionInfo, stream: Stream) -> Result<(), String>;
}

/// The connection's bytes, as `std::io::Read` and `std::io::Write`. Reads
/// block until data arrives; a read of 0 bytes means the peer closed.
#[derive(Debug)]
pub struct Stream {
    input: Rc<InputStream>,
    output: Rc<OutputStream>,
}

impl Stream {
    /// A second handle on the same connection, for writing while another
    /// handle reads (for example inside a `BufReader`).
    #[must_use]
    pub fn clone_writer(&self) -> Self {
        Self {
            input: self.input.clone(),
            output: self.output.clone(),
        }
    }
}

fn io_error(e: StreamError) -> io::Error {
    match e {
        StreamError::Closed => io::Error::from(io::ErrorKind::BrokenPipe),
        StreamError::LastOperationFailed(e) => io::Error::other(e.to_debug_string()),
    }
}

impl io::Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        match self.input.blocking_read(len) {
            Ok(bytes) => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Err(StreamError::Closed) => Ok(0),
            Err(e) => Err(io_error(e)),
        }
    }
}

impl io::Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(WRITE_CHUNK);
        self.output
            .blocking_write_and_flush(&buf[..n])
            .map_err(io_error)?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.blocking_flush().map_err(io_error)
    }
}

/// Export `C` as the component's connection handler.
#[macro_export]
macro_rules! connection {
    ($ty:ty) => {
        struct __PaddockConnection;

        impl $crate::connection::bindings::exports::paddock::connection::handler::Guest
            for __PaddockConnection
        {
            fn serve(
                info: $crate::connection::bindings::exports::paddock::connection::handler::ConnectionInfo,
                input: $crate::connection::bindings::wasi::io::streams::InputStream,
                output: $crate::connection::bindings::wasi::io::streams::OutputStream,
            ) -> ::core::result::Result<(), ::std::string::String> {
                $crate::connection::__serve::<$ty>(info, input, output)
            }
        }

        // The export's symbol (`paddock:connection/handler@...#serve`) has a
        // `:`, which native linkers refuse in a version script; it only
        // means anything in a Wasm component.
        #[cfg(target_family = "wasm")]
        $crate::connection::bindings::export_connection!(
            __PaddockConnection with_types_in $crate::connection::bindings
        );

        /// Keeps the handler reachable in native builds, which have no
        /// export, so it is still compiled and linted there.
        #[cfg(not(target_family = "wasm"))]
        #[doc(hidden)]
        pub fn __paddock_connection_serve(
            info: $crate::connection::bindings::exports::paddock::connection::handler::ConnectionInfo,
            input: $crate::connection::bindings::wasi::io::streams::InputStream,
            output: $crate::connection::bindings::wasi::io::streams::OutputStream,
        ) -> ::core::result::Result<(), ::std::string::String> {
            <__PaddockConnection as $crate::connection::bindings::exports::paddock::connection::handler::Guest>::serve(
                info, input, output,
            )
        }
    };
}

#[doc(hidden)]
pub fn __serve<C: Connection>(
    info: wit::ConnectionInfo,
    input: InputStream,
    output: OutputStream,
) -> Result<(), String> {
    let info = ConnectionInfo {
        listener: info.listener,
        peer: info.peer,
    };
    let stream = Stream {
        input: Rc::new(input),
        output: Rc::new(output),
    };
    C::serve(&mut Ctx::new(), info, stream)
}
