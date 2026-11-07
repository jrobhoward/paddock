//! Sockets for native-process apps (ROADMAP Z8e), on the policy and count
//! a component's sockets get.
//!
//! The node makes every socket. A TCP stream is connected (or accepted)
//! here and passed to the app on its descriptor socket; the node keeps a
//! copy, so readiness can be watched here and the socket shut down when
//! the app closes it: an app that kept a socket after closing it would
//! hold a dead one, so its `max_sockets` count stays honest. Listeners and
//! UDP sockets never leave the node: the app accepts and sends and
//! receives datagrams with requests, and each destination and source is
//! checked here, since a passed UDP socket could send anywhere (Linux's
//! `sendmsg` overrides a connected socket's peer). The runner's seccomp
//! filter refuses `socket`, `connect`, `accept` and `io_uring`, so a passed
//! stream is all an app can use.
//!
//! A connection a listener accepted for the app (ROADMAP Z8g) is kept as
//! a stream the same way, tied to its listener's count instead of
//! `max_sockets`.
//!
//! A readiness notice is a task on a copy of the socket's descriptor that
//! delivers the host event `io-ready` (`{"tag": n}`, as a component's
//! `bulkhead:io` sends) once the socket is readable; it ends with the
//! socket or the instance.

use std::collections::BTreeMap;
use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::OwnedFd;
use std::time::Duration;

use bulkhead_native::protocol as p;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use wasmtime_wasi::sockets::SocketAddrUse;

use super::policy::Policy;
use super::{Held, Instance};
use crate::instance::InstanceHandle;
use crate::listener::Tie;
use crate::message::{Envelope, Info};
use crate::ready::EVENT;

/// The largest datagram received.
const MAX_DATAGRAM: usize = 64 << 10;

/// A native instance's sockets.
pub(super) struct Native {
    /// The app's policy; `None` without the capability granted.
    policy: Option<Policy>,
    handle: InstanceHandle,
    sockets: BTreeMap<u32, Socket>,
    next: u32,
}

/// One socket the node holds for the app.
struct Socket {
    kind: Kind,
    /// The readiness notice waiting, if any.
    notice: Option<Notice>,
    _share: Share,
}

/// What a socket counts against, given back when it closes.
// Held only to be dropped with the socket.
#[allow(dead_code)]
enum Share {
    /// The app's `max_sockets`.
    Sockets(Held),
    /// Its listener's connections.
    Connection(Tie),
}

enum Kind {
    /// The node's copy of a stream the app holds.
    Stream(TcpStream),
    Listener(TcpListener),
    Udp(UdpSocket),
}

/// A readiness task, aborted when replaced or dropped.
struct Notice(tokio::task::JoinHandle<()>);

impl Drop for Notice {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What a request gives back: the answer, and a descriptor to pass first
/// (its [`p::FdRef`] in the answer is filled in once passed).
pub(crate) type Reply = (p::NetAnswer, Option<OwnedFd>);

impl Native {
    pub(super) fn new(policy: Option<Policy>, handle: &InstanceHandle) -> Self {
        Self {
            policy,
            handle: handle.clone(),
            sockets: BTreeMap::new(),
            next: 0,
        }
    }
}

fn other(e: &io::Error) -> p::NetError {
    match e.kind() {
        io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe => p::NetError::Closed,
        _ => p::NetError::Other(e.to_string()),
    }
}

impl Instance {
    /// A native app's socket request; `deadline` bounds a connect.
    pub(crate) async fn native(
        &mut self,
        request: p::NetRequest,
        deadline: Duration,
    ) -> Result<Reply, p::NetError> {
        match request {
            p::NetRequest::Connect(addr) => self.connect(addr, deadline).await,
            p::NetRequest::ConnectHost { host, port } => {
                self.connect_host(&host, port, deadline).await
            }
            p::NetRequest::Listen(addr) => self.listen(addr),
            p::NetRequest::Accept(socket) => self.accept(socket),
            p::NetRequest::UdpBind(addr) => self.udp_bind(addr),
            p::NetRequest::UdpSend { socket, to, data } => self.udp_send(socket, to, &data.0),
            p::NetRequest::UdpRecv(socket) => self.udp_recv(socket),
            p::NetRequest::Notify { socket, tag } => self.notify(socket, tag),
            p::NetRequest::Close(socket) => {
                if let Some(s) = self.native.sockets.remove(&socket)
                    && let Kind::Stream(stream) = &s.kind
                {
                    let _ = stream.shutdown(Shutdown::Both);
                }
                Ok((p::NetAnswer::Unit, None))
            }
        }
    }

    /// Whether the policy allows `addr` for `why`, counting a refusal.
    fn check(&self, addr: SocketAddr, why: SocketAddrUse) -> Result<(), p::NetError> {
        let Some(policy) = &self.native.policy else {
            self.stats.socket_refused();
            return Err(p::NetError::Denied);
        };
        if policy.allows(addr, why) {
            Ok(())
        } else {
            tracing::debug!(%addr, ?why, "socket address refused");
            self.stats.socket_refused();
            Err(p::NetError::Denied)
        }
    }

    /// A share of the app's `max_sockets`.
    fn held(&self) -> Result<Held, p::NetError> {
        if self.native.policy.is_none() {
            return Err(p::NetError::Denied);
        }
        match self.limit.clone().try_acquire_owned() {
            Ok(permit) => Ok(Held::new(permit, &self.stats)),
            Err(_) => {
                self.stats.socket_refused();
                Err(p::NetError::Limited)
            }
        }
    }

    /// Shut down every stream and drop every socket: the process that held
    /// them has gone (passivation), and no later one knows them.
    pub(crate) fn close_native(&mut self) {
        for (_, s) in std::mem::take(&mut self.native.sockets) {
            if let Kind::Stream(stream) = &s.kind {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }

    /// Keep a connection a listener accepted for the app, answering with
    /// the stream to pass and its copy's descriptor.
    pub(crate) fn connection(
        &mut self,
        stream: TcpStream,
        tie: Tie,
    ) -> Result<(p::NetStream, OwnedFd), p::NetError> {
        self.stream(stream, Share::Connection(tie))
    }

    fn keep_native(&mut self, kind: Kind, share: Share) -> u32 {
        let native = &mut self.native;
        let mut id = native.next;
        while native.sockets.contains_key(&id) {
            id = id.wrapping_add(1);
        }
        native.next = id.wrapping_add(1);
        native.sockets.insert(
            id,
            Socket {
                kind,
                notice: None,
                _share: share,
            },
        );
        id
    }

    /// Keep `stream`'s copy and answer with the other, to pass.
    fn stream(
        &mut self,
        stream: TcpStream,
        share: Share,
    ) -> Result<(p::NetStream, OwnedFd), p::NetError> {
        let local = stream.local_addr().map_err(|e| other(&e))?;
        let peer = stream.peer_addr().map_err(|e| other(&e))?;
        // Blocking: the app writes blocking and reads with MSG_DONTWAIT,
        // and both copies share the flag.
        stream.set_nonblocking(false).map_err(|e| other(&e))?;
        let passed = OwnedFd::from(stream.try_clone().map_err(|e| other(&e))?);
        let socket = self.keep_native(Kind::Stream(stream), share);
        Ok((
            p::NetStream {
                socket,
                fd: p::FdRef(0),
                local,
                peer,
            },
            passed,
        ))
    }

    async fn connect(
        &mut self,
        addr: SocketAddr,
        deadline: Duration,
    ) -> Result<Reply, p::NetError> {
        self.check(addr, SocketAddrUse::TcpConnect)?;
        let held = self.held()?;
        let stream = tokio::time::timeout(deadline, tokio::net::TcpStream::connect(addr))
            .await
            .map_err(|_| p::NetError::Other("connect timed out".into()))?
            .map_err(|e| other(&e))?
            .into_std()
            .map_err(|e| other(&e))?;
        let (stream, fd) = self.stream(stream, Share::Sockets(held))?;
        Ok((p::NetAnswer::Stream(stream), Some(fd)))
    }

    async fn connect_host(
        &mut self,
        host: &str,
        port: u16,
        deadline: Duration,
    ) -> Result<Reply, p::NetError> {
        if !self.native.policy.as_ref().is_some_and(Policy::name_lookup) {
            // As wasmtime-wasi reports a lookup it does not allow.
            return Err(p::NetError::Other("permanent-resolver-failure".into()));
        }
        let found: Vec<SocketAddr> =
            tokio::time::timeout(deadline, tokio::net::lookup_host((host, port)))
                .await
                .map_err(|_| p::NetError::Other("name lookup timed out".into()))?
                .map_err(|e| p::NetError::Other(e.to_string()))?
                .collect();
        let mut last = p::NetError::Other(format!("{host} has no address"));
        for addr in found {
            match self.connect(addr, deadline).await {
                Ok(reply) => return Ok(reply),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn listen(&mut self, addr: SocketAddr) -> Result<Reply, p::NetError> {
        self.check(addr, SocketAddrUse::TcpBind)?;
        self.check(addr, SocketAddrUse::TcpListen)?;
        let held = self.held()?;
        let listener = TcpListener::bind(addr).map_err(|e| other(&e))?;
        listener.set_nonblocking(true).map_err(|e| other(&e))?;
        let local = listener.local_addr().map_err(|e| other(&e))?;
        let socket = self.keep_native(Kind::Listener(listener), Share::Sockets(held));
        Ok((p::NetAnswer::Listener { socket, local }, None))
    }

    fn accept(&mut self, socket: u32) -> Result<Reply, p::NetError> {
        let Some(Socket {
            kind: Kind::Listener(listener),
            ..
        }) = self.native.sockets.get(&socket)
        else {
            return Err(p::NetError::Closed);
        };
        let (stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Ok((p::NetAnswer::Accepted(None), None));
            }
            Err(e) => return Err(other(&e)),
        };
        self.check(peer, SocketAddrUse::TcpAccept)?;
        let held = self.held()?;
        let (stream, fd) = self.stream(stream, Share::Sockets(held))?;
        Ok((p::NetAnswer::Accepted(Some(stream)), Some(fd)))
    }

    fn udp_bind(&mut self, addr: SocketAddr) -> Result<Reply, p::NetError> {
        self.check(addr, SocketAddrUse::UdpBind)?;
        let held = self.held()?;
        let udp = UdpSocket::bind(addr).map_err(|e| other(&e))?;
        udp.set_nonblocking(true).map_err(|e| other(&e))?;
        let local = udp.local_addr().map_err(|e| other(&e))?;
        let socket = self.keep_native(Kind::Udp(udp), Share::Sockets(held));
        Ok((p::NetAnswer::Udp { socket, local }, None))
    }

    fn udp(&self, socket: u32) -> Result<&UdpSocket, p::NetError> {
        match self.native.sockets.get(&socket) {
            Some(Socket {
                kind: Kind::Udp(udp),
                ..
            }) => Ok(udp),
            _ => Err(p::NetError::Closed),
        }
    }

    fn udp_send(&self, socket: u32, to: SocketAddr, data: &[u8]) -> Result<Reply, p::NetError> {
        self.check(to, SocketAddrUse::UdpSend)?;
        self.udp(socket)?.send_to(data, to).map_err(|e| other(&e))?;
        Ok((p::NetAnswer::Unit, None))
    }

    /// A datagram from a source the policy allows; others are dropped.
    fn udp_recv(&self, socket: u32) -> Result<Reply, p::NetError> {
        let udp = self.udp(socket)?;
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            match udp.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if self.check(from, SocketAddrUse::UdpReceive).is_ok() {
                        buf.truncate(n);
                        return Ok((p::NetAnswer::Datagram(Some((from, p::Bytes(buf)))), None));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Ok((p::NetAnswer::Datagram(None), None));
                }
                Err(e) => return Err(other(&e)),
            }
        }
    }

    fn notify(&mut self, socket: u32, tag: u64) -> Result<Reply, p::NetError> {
        let handle = self.native.handle.clone();
        let Some(s) = self.native.sockets.get_mut(&socket) else {
            return Err(p::NetError::Closed);
        };
        let fd = match &s.kind {
            Kind::Stream(t) => t.try_clone().map(OwnedFd::from),
            Kind::Listener(l) => l.try_clone().map(OwnedFd::from),
            Kind::Udp(u) => u.try_clone().map(OwnedFd::from),
        }
        .map_err(|e| other(&e))?;
        // SAFETY: `fd` is an `OwnedFd` the `AsyncFd` takes and keeps until
        // it drops: open, the same file description, and the same number
        // from `as_raw_fd` for its whole life.
        let watched = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE) }
            .map_err(|e| p::NetError::Other(e.to_string()))?;
        s.notice = Some(Notice(tokio::spawn(async move {
            if watched.readable().await.is_ok() {
                let body = serde_json::json!({ "tag": tag });
                handle.signal(Info::HostEvent {
                    name: EVENT.to_string(),
                    msg: Envelope::new("application/json", body.to_string()),
                });
            }
        })));
        Ok((p::NetAnswer::Unit, None))
    }
}
