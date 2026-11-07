//! Connection handlers (cargo feature `connection`): code that serves the
//! connections accepted on the app's listeners.
//!
//! Two traits:
//!
//! - [`Handler`] takes each connection's bytes as they arrive and answers
//!   through an [`Output`]. It builds both ways: as a component
//!   (`bulkhead:connection`), each connection gets its own instance; as a
//!   native program (`connection_handler!`'s `bulkhead_main`, ROADMAP
//!   Z8g), each instance of the app's pool holds many connections, and the
//!   node says when one has bytes to read.
//! - [`Connection`] serves one connection to its end with blocking reads
//!   and writes: a component only, an instance per connection.
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
//! use bulkhead_sdk::connection::{Connection, ConnectionInfo, Stream};
//! use bulkhead_sdk::Ctx;
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
//! bulkhead_sdk::connection!(Echo);
//! ```
//!
//! Returning closes the connection; an error closes it as a crash. A
//! connection with no bytes in either direction for the listener's idle
//! timeout is closed by the host. A component handler has a pid but no
//! mailbox: messages sent to it are dropped.
//!
//! The same line protocol as a [`Handler`], which builds both ways:
//!
//! ```ignore
//! use std::io::Write;
//! use bulkhead_sdk::connection::{ConnectionInfo, Flow, Handler, Output};
//! use bulkhead_sdk::Ctx;
//!
//! struct Echo {
//!     buf: Vec<u8>,
//! }
//!
//! impl Handler for Echo {
//!     fn open(_ctx: &mut Ctx, _info: &ConnectionInfo) -> Result<Self, String> {
//!         Ok(Echo { buf: Vec::new() })
//!     }
//!
//!     fn data(&mut self, _ctx: &mut Ctx, bytes: &[u8], out: &mut Output<'_>) -> Result<Flow, String> {
//!         self.buf.extend_from_slice(bytes);
//!         while let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
//!             let line: Vec<u8> = self.buf.drain(..=end).collect();
//!             out.write_all(&line).map_err(|e| e.to_string())?;
//!         }
//!         Ok(Flow::Continue)
//!     }
//! }
//!
//! bulkhead_sdk::connection_handler!(Echo);
//! ```
//!
//! A native build is a binary calling `bulkhead_main()`, with the manifest's
//! `kind = "native-process"`. There, an error from [`Handler::data`]
//! closes that connection only (logged as a warning), a panic crashes the
//! instance and closes every connection it holds, and the instance has a
//! mailbox like any app's: calls to it are answered with an error text.
//! Writes block the callback until the kernel takes the bytes, counted
//! against `callback_deadline_ms`.

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
            "../../wit/sql",
            "wit",
        ],
        world: "bulkhead:sdk/connection",
        pub_export_macro: true,
        export_macro_name: "export_connection",
        default_bindings_module: "bulkhead_sdk::connection::bindings",
        with: {
            "bulkhead:runtime/types@0.1.0": crate::bindings::bulkhead::runtime::types,
            "bulkhead:runtime/process@0.1.0": crate::bindings::bulkhead::runtime::process,
        },
        generate_all,
    });
}

use bindings::exports::bulkhead::connection::handler as wit;
use bindings::wasi::io::streams::{InputStream, OutputStream, StreamError};

/// Bytes `wasi:io` writes per blocking call.
const WRITE_CHUNK: usize = 4096;

/// The most bytes a [`Handler`] gets in one [`Handler::data`].
pub const READ_MAX: usize = 64 * 1024;

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

/// What a connection does after [`Handler::data`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Wait for more bytes.
    Continue,
    /// Close the connection.
    Close,
}

/// A connection handler taking each connection's bytes as they arrive;
/// `Self` is one connection's state. Builds as a component and as a
/// native program (see the module docs).
pub trait Handler: Sized + 'static {
    /// A new connection.
    ///
    /// # Errors
    ///
    /// A message closes the connection at once.
    fn open(ctx: &mut Ctx, info: &ConnectionInfo) -> Result<Self, String>;

    /// Up to [`READ_MAX`] bytes the peer sent; answer through `out`.
    ///
    /// # Errors
    ///
    /// A message closes the connection: as a crash in a component, with a
    /// warning in a native program.
    fn data(&mut self, ctx: &mut Ctx, bytes: &[u8], out: &mut Output<'_>) -> Result<Flow, String>;

    /// The peer closed the connection (or the node did, after the idle
    /// timeout).
    fn closed(&mut self, _ctx: &mut Ctx) {}
}

/// Where a [`Handler`] writes to its peer. Writes block until the bytes
/// are taken.
#[derive(Debug)]
pub struct Output<'a> {
    to: To<'a>,
}

#[derive(Debug)]
enum To<'a> {
    // Not built in a native program, which writes to its stream.
    #[cfg_attr(all(unix, not(target_arch = "wasm32")), allow(dead_code))]
    Wasi(&'a OutputStream),
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    Native(&'a mut std::net::TcpStream),
}

impl io::Write for Output<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.to {
            To::Wasi(output) => {
                let n = buf.len().min(WRITE_CHUNK);
                output
                    .blocking_write_and_flush(&buf[..n])
                    .map_err(io_error)?;
                Ok(n)
            }
            #[cfg(all(unix, not(target_arch = "wasm32")))]
            To::Native(stream) => io::Write::write(stream, buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.to {
            To::Wasi(output) => output.blocking_flush().map_err(io_error),
            #[cfg(all(unix, not(target_arch = "wasm32")))]
            To::Native(stream) => io::Write::flush(stream),
        }
    }
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

/// Export `H` (a [`Handler`]) as the component's connection handler, and,
/// in a native build, add `bulkhead_main()` to serve it as a native
/// program: call it from `main` in a binary of the app's crate.
#[macro_export]
macro_rules! connection_handler {
    ($ty:ty) => {
        struct __BulkheadConnection;

        impl $crate::connection::bindings::exports::bulkhead::connection::handler::Guest
            for __BulkheadConnection
        {
            fn serve(
                info: $crate::connection::bindings::exports::bulkhead::connection::handler::ConnectionInfo,
                input: $crate::connection::bindings::wasi::io::streams::InputStream,
                output: $crate::connection::bindings::wasi::io::streams::OutputStream,
            ) -> ::core::result::Result<(), ::std::string::String> {
                $crate::connection::__serve_handler::<$ty>(info, input, output)
            }
        }

        #[cfg(target_family = "wasm")]
        $crate::connection::bindings::export_connection!(
            __BulkheadConnection with_types_in $crate::connection::bindings
        );

        /// Keeps the component's handler reachable in native builds,
        /// which have no export, so it is still compiled and linted there.
        #[cfg(not(target_family = "wasm"))]
        #[doc(hidden)]
        pub fn __bulkhead_connection_serve(
            info: $crate::connection::bindings::exports::bulkhead::connection::handler::ConnectionInfo,
            input: $crate::connection::bindings::wasi::io::streams::InputStream,
            output: $crate::connection::bindings::wasi::io::streams::OutputStream,
        ) -> ::core::result::Result<(), ::std::string::String> {
            <__BulkheadConnection as $crate::connection::bindings::exports::bulkhead::connection::handler::Guest>::serve(
                info, input, output,
            )
        }

        /// Serve this handler as a native program: call from `main` in a
        /// binary of the app's crate, run with `native.program =
        /// "executable"`.
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        pub fn bulkhead_main() -> ::std::process::ExitCode {
            $crate::connection::native::serve::<$ty>()
        }
    };
}

/// Export `C` as the component's connection handler.
#[macro_export]
macro_rules! connection {
    ($ty:ty) => {
        struct __BulkheadConnection;

        impl $crate::connection::bindings::exports::bulkhead::connection::handler::Guest
            for __BulkheadConnection
        {
            fn serve(
                info: $crate::connection::bindings::exports::bulkhead::connection::handler::ConnectionInfo,
                input: $crate::connection::bindings::wasi::io::streams::InputStream,
                output: $crate::connection::bindings::wasi::io::streams::OutputStream,
            ) -> ::core::result::Result<(), ::std::string::String> {
                $crate::connection::__serve::<$ty>(info, input, output)
            }
        }

        // The export's symbol (`bulkhead:connection/handler@...#serve`) has a
        // `:`, which native linkers refuse in a version script; it only
        // means anything in a Wasm component.
        #[cfg(target_family = "wasm")]
        $crate::connection::bindings::export_connection!(
            __BulkheadConnection with_types_in $crate::connection::bindings
        );

        /// Keeps the handler reachable in native builds, which have no
        /// export, so it is still compiled and linted there.
        #[cfg(not(target_family = "wasm"))]
        #[doc(hidden)]
        pub fn __bulkhead_connection_serve(
            info: $crate::connection::bindings::exports::bulkhead::connection::handler::ConnectionInfo,
            input: $crate::connection::bindings::wasi::io::streams::InputStream,
            output: $crate::connection::bindings::wasi::io::streams::OutputStream,
        ) -> ::core::result::Result<(), ::std::string::String> {
            <__BulkheadConnection as $crate::connection::bindings::exports::bulkhead::connection::handler::Guest>::serve(
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

#[doc(hidden)]
pub fn __serve_handler<H: Handler>(
    info: wit::ConnectionInfo,
    input: InputStream,
    output: OutputStream,
) -> Result<(), String> {
    let info = ConnectionInfo {
        listener: info.listener,
        peer: info.peer,
    };
    let mut ctx = Ctx::new();
    let mut handler = H::open(&mut ctx, &info)?;
    let len = u64::try_from(READ_MAX).unwrap_or(u64::MAX);
    loop {
        match input.blocking_read(len) {
            Ok(bytes) if bytes.is_empty() => {}
            Ok(bytes) => {
                let mut out = Output {
                    to: To::Wasi(&output),
                };
                if handler.data(&mut ctx, &bytes, &mut out)? == Flow::Close {
                    return Ok(());
                }
            }
            Err(StreamError::Closed) => {
                handler.closed(&mut ctx);
                return Ok(());
            }
            Err(e) => return Err(io_error(e).to_string()),
        }
    }
}

/// A [`Handler`] served as a native program: an instance holds many
/// connections, each read when the node says it has bytes.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub mod native {
    use std::collections::BTreeMap;
    use std::io;
    use std::os::fd::AsFd;
    use std::process::ExitCode;

    use bulkhead_native::app as n;
    use bulkhead_native::protocol as p;

    use super::{ConnectionInfo, Flow, Handler, Output, READ_MAX, To};
    use crate::Ctx;
    use crate::log::{self, Level};

    /// The host event a readable stream sends (`bulkhead:io`'s).
    const READY_EVENT: &str = "io-ready";

    /// One connection: the handler's state and the passed stream.
    struct Open<H> {
        handler: H,
        socket: u32,
        stream: std::net::TcpStream,
    }

    impl<H> Drop for Open<H> {
        fn drop(&mut self) {
            n::close_socket_quietly(self.socket);
        }
    }

    /// An instance's connections, by the tag their readiness comes with.
    struct Connections<H> {
        open: BTreeMap<u64, Open<H>>,
        next: u64,
    }

    fn notify(socket: u32, tag: u64) -> bool {
        n::Ctx::current()
            .net(p::NetRequest::Notify { socket, tag })
            .is_ok()
    }

    impl<H: Handler> Connections<H> {
        /// Read what `tag`'s stream has and hand it to its handler; then
        /// wait for more, or close it.
        fn readable(&mut self, tag: u64) {
            let Some(open) = self.open.get_mut(&tag) else {
                return;
            };
            let mut ctx = Ctx::new();
            let mut buf = vec![0u8; READ_MAX];
            let flow = match n::recv_dontwait(open.stream.as_fd(), &mut buf) {
                Ok(0) => {
                    open.handler.closed(&mut ctx);
                    Ok(Flow::Close)
                }
                Ok(read) => {
                    let mut out = Output {
                        to: To::Native(&mut open.stream),
                    };
                    open.handler.data(&mut ctx, &buf[..read], &mut out)
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(Flow::Continue),
                Err(e) => {
                    open.handler.closed(&mut ctx);
                    Err(e.to_string())
                }
            };
            match flow {
                Ok(Flow::Continue) if notify(open.socket, tag) => {}
                Ok(_) => {
                    self.open.remove(&tag);
                }
                Err(why) => {
                    log::write(Level::Warn, &format!("connection closed: {why}"));
                    self.open.remove(&tag);
                }
            }
        }
    }

    impl<H: Handler> n::App for Connections<H> {
        fn init(_ctx: &mut n::Ctx, _args: Vec<u8>) -> Result<Self, String> {
            Ok(Self {
                open: BTreeMap::new(),
                next: 0,
            })
        }

        fn handle_call(
            &mut self,
            _ctx: &mut n::Ctx,
            _call: n::CallRef,
            _msg: p::Envelope,
        ) -> Option<p::Envelope> {
            Some(p::Envelope::new(
                "text/plain",
                "a connection handler takes no calls",
            ))
        }

        fn handle_info(&mut self, _ctx: &mut n::Ctx, info: p::Info) {
            #[derive(serde::Deserialize)]
            struct Ready {
                tag: u64,
            }
            let p::Info::HostEvent { name, msg } = info else {
                return;
            };
            if name == crate::log::LEVEL_EVENT {
                crate::log::set_from_host(&msg.body);
            } else if name == READY_EVENT
                && let Ok(ready) = serde_json::from_slice::<Ready>(&msg.body)
            {
                self.readable(ready.tag);
            }
        }

        fn handle_connection(&mut self, _ctx: &mut n::Ctx, listener: String, stream: p::NetStream) {
            let socket = stream.socket;
            let peer = stream.peer.to_string();
            let Ok(fd) = n::take_fd(stream.fd) else {
                n::close_socket_quietly(socket);
                return;
            };
            let info = ConnectionInfo { listener, peer };
            let open = match H::open(&mut Ctx::new(), &info) {
                Ok(handler) => Open {
                    handler,
                    socket,
                    stream: std::net::TcpStream::from(fd),
                },
                Err(why) => {
                    log::write(Level::Warn, &format!("connection refused: {why}"));
                    n::close_socket_quietly(socket);
                    return;
                }
            };
            let tag = self.next;
            self.next += 1;
            if notify(socket, tag) {
                self.open.insert(tag, open);
            }
        }
    }

    /// Serve `H` as a native program until the node stops it.
    #[must_use]
    pub fn serve<H: Handler>() -> ExitCode {
        n::serve::<Connections<H>>()
    }
}
