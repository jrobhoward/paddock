//! TCP from apps (`wasi:sockets`), with readiness as messages
//! (`paddock:io`), as Erlang's `{active, once}`.
//!
//! An instance never blocks waiting for a peer between callbacks: it asks
//! to hear when a stream has bytes ([`TcpStream::notify_readable`]) or a
//! listener has a connection ([`TcpListener::notify_acceptable`]), returns,
//! and handles the host event in `handle_info` ([`ready_tag`]), reading
//! without blocking. A notice comes once per request; ask again after
//! reading. Connecting and writing block within the callback, as host time
//! (not counted against the callback deadline).
//!
//! The manifest declares `wasi:sockets` with the addresses the app may
//! connect to, bind and listen on; anything else fails with
//! [`NetError::Denied`].
//!
//! ```no_run
//! use paddock_sdk::net::{self, TcpStream};
//! use paddock_sdk::{Ctx, Info};
//!
//! # fn demo(ctx: &mut Ctx, info: Info) -> Result<(), net::NetError> {
//! let mut stream = TcpStream::connect("10.0.0.5:1883".parse().unwrap())?;
//! stream.write_all(b"hello")?;
//! stream.notify_readable(1)?;
//! // Later, in handle_info:
//! if net::ready_tag(&info) == Some(1) {
//!     let bytes = stream.read(64 * 1024)?;
//!     stream.notify_readable(1)?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! UDP and lower-level use go through [`wasip2::sockets`] directly, with
//! [`notify`] on any pollable.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub use wasip2;
use wasip2::io::poll::Pollable;
use wasip2::io::streams::{InputStream, OutputStream, StreamError};
use wasip2::sockets::instance_network::instance_network;
use wasip2::sockets::network::{
    ErrorCode, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress,
};
use wasip2::sockets::tcp::TcpSocket;
use wasip2::sockets::tcp_create_socket::create_tcp_socket;

use crate::Info;

#[doc(hidden)]
#[allow(missing_docs, clippy::too_many_arguments)]
pub mod bindings {
    wit_bindgen::generate!({
        path: ["../../wit/vendor/wasi-io", "../../wit/io"],
        world: "paddock:io/imports",
        with: {
            "wasi:io/poll@0.2.12": wasip2::io::poll,
        },
    });
}

use bindings::paddock::io::ready;

/// The host event a ready pollable sends.
pub const READY_EVENT: &str = "io-ready";

/// Why a socket operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetError {
    /// Not allowed: the capability is not granted, or the address is not
    /// in the manifest's rules.
    Denied,
    /// The app already holds `max_sockets` sockets, or the instance waits
    /// on as many pollables as it may.
    Limited,
    /// The peer closed the stream.
    Closed,
    /// Anything else, as the host reported it.
    Other(String),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied => f.write_str("denied"),
            Self::Limited => f.write_str("limited"),
            Self::Closed => f.write_str("closed"),
            Self::Other(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for NetError {}

impl From<ErrorCode> for NetError {
    fn from(code: ErrorCode) -> Self {
        match code {
            ErrorCode::AccessDenied => Self::Denied,
            ErrorCode::NewSocketLimit => Self::Limited,
            ErrorCode::ConnectionReset | ErrorCode::ConnectionAborted => Self::Closed,
            other => Self::Other(format!("{other:?}")),
        }
    }
}

impl From<StreamError> for NetError {
    fn from(e: StreamError) -> Self {
        match e {
            StreamError::Closed => Self::Closed,
            StreamError::LastOperationFailed(e) => Self::Other(e.to_debug_string()),
        }
    }
}

/// Send the host event [`READY_EVENT`] with `tag` once `pollable` is
/// ready, once. Keep the pollable alive until then: dropping it withdraws
/// the notice.
///
/// # Errors
///
/// [`NetError::Limited`] when the instance already waits on 1,024.
pub fn notify(pollable: &Pollable, tag: u64) -> Result<(), NetError> {
    ready::notify(pollable, tag).map_err(|ready::Error::LimitReached| NetError::Limited)
}

/// Withdraw the notice for `pollable`; whether one was waiting.
pub fn cancel(pollable: &Pollable) -> bool {
    ready::cancel(pollable)
}

/// The tag of a [`READY_EVENT`], if `info` is one.
#[must_use]
pub fn ready_tag(info: &Info) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct Ready {
        tag: u64,
    }
    match info {
        Info::Event { name, msg } if name == READY_EVENT => {
            serde_json::from_slice::<Ready>(&msg.body)
                .ok()
                .map(|r| r.tag)
        }
        _ => None,
    }
}

fn to_wasi(addr: SocketAddr) -> IpSocketAddress {
    match addr {
        SocketAddr::V4(a) => {
            let [a0, a1, a2, a3] = a.ip().octets();
            IpSocketAddress::Ipv4(Ipv4SocketAddress {
                port: a.port(),
                address: (a0, a1, a2, a3),
            })
        }
        SocketAddr::V6(a) => {
            let [s0, s1, s2, s3, s4, s5, s6, s7] = a.ip().segments();
            IpSocketAddress::Ipv6(Ipv6SocketAddress {
                port: a.port(),
                flow_info: a.flowinfo(),
                address: (s0, s1, s2, s3, s4, s5, s6, s7),
                scope_id: a.scope_id(),
            })
        }
    }
}

fn from_wasi(addr: IpSocketAddress) -> SocketAddr {
    match addr {
        IpSocketAddress::Ipv4(a) => {
            let (a0, a1, a2, a3) = a.address;
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a0, a1, a2, a3)), a.port)
        }
        IpSocketAddress::Ipv6(a) => {
            let (s0, s1, s2, s3, s4, s5, s6, s7) = a.address;
            SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(s0, s1, s2, s3, s4, s5, s6, s7)),
                a.port,
            )
        }
    }
}

fn family(addr: SocketAddr) -> IpAddressFamily {
    if addr.is_ipv4() {
        IpAddressFamily::Ipv4
    } else {
        IpAddressFamily::Ipv6
    }
}

/// Run a start/finish pair, blocking on the socket until it finishes.
fn finish<T>(
    socket: &TcpSocket,
    mut step: impl FnMut() -> Result<T, ErrorCode>,
) -> Result<T, NetError> {
    loop {
        match step() {
            Err(ErrorCode::WouldBlock) => socket.subscribe().block(),
            done => return done.map_err(NetError::from),
        }
    }
}

/// A connected TCP stream.
///
/// Fields drop in order: the pollable before the streams, the streams
/// before the socket, as `wasi:io` requires.
#[derive(Debug)]
pub struct TcpStream {
    armed: Option<Pollable>,
    input: InputStream,
    output: OutputStream,
    socket: TcpSocket,
}

impl TcpStream {
    /// Connect to `addr`, blocking until connected.
    ///
    /// # Errors
    ///
    /// [`NetError::Denied`] if the manifest does not allow the address,
    /// [`NetError::Limited`] over `max_sockets`, otherwise what the
    /// connection failed with.
    pub fn connect(addr: SocketAddr) -> Result<Self, NetError> {
        let network = instance_network();
        let socket = create_tcp_socket(family(addr))?;
        socket.start_connect(&network, to_wasi(addr))?;
        let (input, output) = finish(&socket, || socket.finish_connect())?;
        Ok(Self {
            armed: None,
            input,
            output,
            socket,
        })
    }

    /// Connect to the first address `host` resolves to that accepts.
    ///
    /// # Errors
    ///
    /// As [`TcpStream::connect`]. A name lookup the manifest does not
    /// allow (no `connect` rules) fails as an unresolvable name does
    /// (`permanent-resolver-failure`), as wasmtime-wasi reports it.
    pub fn connect_host(host: &str, port: u16) -> Result<Self, NetError> {
        let network = instance_network();
        let found = wasip2::sockets::ip_name_lookup::resolve_addresses(&network, host)?;
        let mut last = NetError::Other(format!("{host} has no address"));
        loop {
            let next = match found.resolve_next_address() {
                Err(ErrorCode::WouldBlock) => {
                    found.subscribe().block();
                    continue;
                }
                other => other?,
            };
            let Some(ip) = next else {
                return Err(last);
            };
            let addr = from_wasi(match ip {
                wasip2::sockets::network::IpAddress::Ipv4(a) => {
                    IpSocketAddress::Ipv4(Ipv4SocketAddress { port, address: a })
                }
                wasip2::sockets::network::IpAddress::Ipv6(a) => {
                    IpSocketAddress::Ipv6(Ipv6SocketAddress {
                        port,
                        flow_info: 0,
                        address: a,
                        scope_id: 0,
                    })
                }
            });
            match Self::connect(addr) {
                Ok(stream) => return Ok(stream),
                Err(e) => last = e,
            }
        }
    }

    /// Up to `max` bytes already received, without blocking: empty when
    /// none have arrived.
    ///
    /// # Errors
    ///
    /// [`NetError::Closed`] once the peer has closed and every byte was read.
    pub fn read(&mut self, max: usize) -> Result<Vec<u8>, NetError> {
        let max = u64::try_from(max).unwrap_or(u64::MAX);
        Ok(self.input.read(max)?)
    }

    /// Write all of `bytes`, blocking until the host has taken them.
    ///
    /// # Errors
    ///
    /// [`NetError::Closed`] if the peer closed the stream.
    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), NetError> {
        // `blocking-write-and-flush` takes at most 4,096 bytes a call.
        for chunk in bytes.chunks(4096) {
            self.output.blocking_write_and_flush(chunk)?;
        }
        Ok(())
    }

    /// Send [`READY_EVENT`] with `tag` once bytes arrive (or the peer
    /// closes), once.
    ///
    /// # Errors
    ///
    /// [`NetError::Limited`] when the instance already waits on 1,024.
    pub fn notify_readable(&mut self, tag: u64) -> Result<(), NetError> {
        let pollable = self.input.subscribe();
        notify(&pollable, tag)?;
        self.armed = Some(pollable);
        Ok(())
    }

    /// The peer's address.
    ///
    /// # Errors
    ///
    /// What the host reported.
    pub fn peer_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(from_wasi(self.socket.remote_address()?))
    }

    /// The local address.
    ///
    /// # Errors
    ///
    /// What the host reported.
    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(from_wasi(self.socket.local_address()?))
    }
}

/// A listening TCP socket.
#[derive(Debug)]
pub struct TcpListener {
    armed: Option<Pollable>,
    socket: TcpSocket,
}

impl TcpListener {
    /// Bind `addr` and listen. Port 0 picks a free port
    /// ([`TcpListener::local_addr`]).
    ///
    /// # Errors
    ///
    /// [`NetError::Denied`] unless the manifest sets `listen` and a `bind`
    /// rule allows the address.
    pub fn bind(addr: SocketAddr) -> Result<Self, NetError> {
        let network = instance_network();
        let socket = create_tcp_socket(family(addr))?;
        socket.start_bind(&network, to_wasi(addr))?;
        finish(&socket, || socket.finish_bind())?;
        socket.start_listen()?;
        finish(&socket, || socket.finish_listen())?;
        Ok(Self {
            armed: None,
            socket,
        })
    }

    /// A connection already waiting, without blocking.
    ///
    /// # Errors
    ///
    /// [`NetError::Limited`] over `max_sockets`, otherwise what the host
    /// reported.
    pub fn accept(&mut self) -> Result<Option<TcpStream>, NetError> {
        match self.socket.accept() {
            Ok((socket, input, output)) => Ok(Some(TcpStream {
                armed: None,
                input,
                output,
                socket,
            })),
            Err(ErrorCode::WouldBlock) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Send [`READY_EVENT`] with `tag` once a connection waits, once.
    ///
    /// # Errors
    ///
    /// [`NetError::Limited`] when the instance already waits on 1,024.
    pub fn notify_acceptable(&mut self, tag: u64) -> Result<(), NetError> {
        let pollable = self.socket.subscribe();
        notify(&pollable, tag)?;
        self.armed = Some(pollable);
        Ok(())
    }

    /// The bound address.
    ///
    /// # Errors
    ///
    /// What the host reported.
    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(from_wasi(self.socket.local_address()?))
    }
}
