//! TCP streams and listeners: the node makes each socket on the manifest's
//! rules and passes a stream on the descriptor socket; it keeps listeners
//! itself and watches readiness. Reading and writing are the app's own
//! system calls on the passed stream.

use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::fd::AsFd;

use bulkhead_native::app::{self, Ctx};
use bulkhead_native::protocol as p;

use super::NetError;

fn error(e: p::NetError) -> NetError {
    match e {
        p::NetError::Denied => NetError::Denied,
        p::NetError::Limited => NetError::Limited,
        p::NetError::Closed => NetError::Closed,
        p::NetError::Other(why) => NetError::Other(why),
    }
}

fn io_error(e: &io::Error) -> NetError {
    match e.kind() {
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted => NetError::Closed,
        _ => NetError::Other(e.to_string()),
    }
}

fn ask(request: p::NetRequest) -> Result<p::NetAnswer, NetError> {
    Ctx::current().net(request).map_err(error)
}

fn unexpected(answer: &p::NetAnswer) -> NetError {
    NetError::Other(format!("unexpected answer from the node: {answer:?}"))
}

/// A connected TCP stream.
#[derive(Debug)]
pub struct TcpStream {
    socket: u32,
    stream: std::net::TcpStream,
    local: SocketAddr,
    peer: SocketAddr,
}

impl TcpStream {
    fn adopt(s: p::NetStream) -> Result<Self, NetError> {
        let fd = app::take_fd(s.fd).map_err(|e| io_error(&e))?;
        Ok(Self {
            socket: s.socket,
            stream: std::net::TcpStream::from(fd),
            local: s.local,
            peer: s.peer,
        })
    }

    fn from_answer(answer: p::NetAnswer) -> Result<Self, NetError> {
        match answer {
            p::NetAnswer::Stream(s) => Self::adopt(s),
            other => Err(unexpected(&other)),
        }
    }

    /// Connect to `addr`, blocking until connected.
    ///
    /// # Errors
    ///
    /// [`NetError::Denied`] if the manifest does not allow the address,
    /// [`NetError::Limited`] over `max_sockets`, otherwise what the
    /// connection failed with.
    pub fn connect(addr: SocketAddr) -> Result<Self, NetError> {
        Self::from_answer(ask(p::NetRequest::Connect(addr))?)
    }

    /// Connect to the first address `host` resolves to that accepts.
    ///
    /// # Errors
    ///
    /// As [`TcpStream::connect`]. A name lookup the manifest does not
    /// allow (no `connect` rules) fails with `permanent-resolver-failure`,
    /// as in a component.
    pub fn connect_host(host: &str, port: u16) -> Result<Self, NetError> {
        Self::from_answer(ask(p::NetRequest::ConnectHost {
            host: host.into(),
            port,
        })?)
    }

    /// Up to `max` bytes already received, without blocking: empty when
    /// none have arrived.
    ///
    /// # Errors
    ///
    /// [`NetError::Closed`] once the peer has closed and every byte was read.
    pub fn read(&mut self, max: usize) -> Result<Vec<u8>, NetError> {
        let mut buf = vec![0u8; max];
        match app::recv_dontwait(self.stream.as_fd(), &mut buf) {
            Ok(0) if max > 0 => Err(NetError::Closed),
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(Vec::new()),
            Err(e) => Err(io_error(&e)),
        }
    }

    /// Write all of `bytes`, blocking until the kernel has taken them.
    ///
    /// # Errors
    ///
    /// [`NetError::Closed`] if the peer closed the stream.
    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), NetError> {
        self.stream.write_all(bytes).map_err(|e| io_error(&e))
    }

    /// Send [`READY_EVENT`](super::READY_EVENT) with `tag` once bytes
    /// arrive (or the peer closes), once.
    ///
    /// # Errors
    ///
    /// [`NetError::Closed`] if the socket was closed.
    pub fn notify_readable(&mut self, tag: u64) -> Result<(), NetError> {
        ask(p::NetRequest::Notify {
            socket: self.socket,
            tag,
        })
        .map(|_| ())
    }

    /// The peer's address.
    ///
    /// # Errors
    ///
    /// None natively; kept for the component build's signature.
    pub fn peer_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(self.peer)
    }

    /// The local address.
    ///
    /// # Errors
    ///
    /// None natively; kept for the component build's signature.
    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(self.local)
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        app::close_socket_quietly(self.socket);
    }
}

/// A listening TCP socket, held by the node.
#[derive(Debug)]
pub struct TcpListener {
    socket: u32,
    local: SocketAddr,
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
        match ask(p::NetRequest::Listen(addr))? {
            p::NetAnswer::Listener { socket, local } => Ok(Self { socket, local }),
            other => Err(unexpected(&other)),
        }
    }

    /// A connection already waiting, without blocking.
    ///
    /// # Errors
    ///
    /// [`NetError::Limited`] over `max_sockets`, otherwise what the node
    /// reported.
    pub fn accept(&mut self) -> Result<Option<TcpStream>, NetError> {
        match ask(p::NetRequest::Accept(self.socket))? {
            p::NetAnswer::Accepted(Some(s)) => TcpStream::adopt(s).map(Some),
            p::NetAnswer::Accepted(None) => Ok(None),
            other => Err(unexpected(&other)),
        }
    }

    /// Send [`READY_EVENT`](super::READY_EVENT) with `tag` once a
    /// connection waits, once.
    ///
    /// # Errors
    ///
    /// [`NetError::Closed`] if the listener was closed.
    pub fn notify_acceptable(&mut self, tag: u64) -> Result<(), NetError> {
        ask(p::NetRequest::Notify {
            socket: self.socket,
            tag,
        })
        .map(|_| ())
    }

    /// The bound address.
    ///
    /// # Errors
    ///
    /// None natively; kept for the component build's signature.
    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(self.local)
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        app::close_socket_quietly(self.socket);
    }
}
