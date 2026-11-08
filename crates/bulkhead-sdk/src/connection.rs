//! Connection handlers (cargo feature `connection`): code that serves the
//! connections accepted on the app's listeners.
//!
//! A [`Handler`] takes each connection's bytes as they arrive and answers
//! through an [`Output`]. Each instance of the app's pool holds many
//! connections, and the node says when one has bytes to read.
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
//! A handler can call and cast to app instances through [`Ctx`] (finding
//! them with [`Ctx::whereis`]), which is how a connection hands work to an
//! app.
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
//! The program is a binary calling `bulkhead_main()`. An error from
//! [`Handler::data`] closes that connection only (logged as a warning), a
//! panic crashes the instance and closes every connection it holds, and the
//! instance has a mailbox like any app's: calls to it are answered with an
//! error text. A connection with no bytes in either direction for the
//! listener's idle timeout is closed by the node. Writes block the callback
//! until the kernel takes the bytes, counted against `callback_deadline_ms`.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::AsFd;
use std::process::ExitCode;

use bulkhead_native::app as n;
use bulkhead_native::protocol as p;

use crate::Ctx;
use crate::log::{self, Level};

/// The most bytes a [`Handler`] gets in one [`Handler::data`].
pub const READ_MAX: usize = 64 * 1024;

/// The host event a readable stream sends.
const READY_EVENT: &str = "io-ready";

/// What the host knows about a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// The listener's name in the manifest.
    pub listener: String,
    /// The peer's address, `ip:port`.
    pub peer: String,
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
/// `Self` is one connection's state.
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
    /// A message closes the connection, with a warning.
    fn data(&mut self, ctx: &mut Ctx, bytes: &[u8], out: &mut Output<'_>) -> Result<Flow, String>;

    /// The peer closed the connection (or the node did, after the idle
    /// timeout).
    fn closed(&mut self, _ctx: &mut Ctx) {}
}

/// Where a [`Handler`] writes to its peer. Writes block until the bytes
/// are taken.
#[derive(Debug)]
pub struct Output<'a> {
    stream: &'a mut std::net::TcpStream,
}

impl io::Write for Output<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(self.stream, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(self.stream)
    }
}

/// Generate `bulkhead_main()` to serve `H` (a [`Handler`]): call it from
/// `main` in a binary of the app's crate.
#[macro_export]
macro_rules! connection_handler {
    ($ty:ty) => {
        /// Serve this handler until the node stops it: call from `main` in
        /// a binary of the app's crate.
        pub fn bulkhead_main() -> ::std::process::ExitCode {
            $crate::connection::serve::<$ty>()
        }
    };
}

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
                    stream: &mut open.stream,
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

/// Serve `H` until the node stops it.
#[doc(hidden)]
#[must_use]
pub fn serve<H: Handler>() -> ExitCode {
    n::serve::<Connections<H>>()
}
