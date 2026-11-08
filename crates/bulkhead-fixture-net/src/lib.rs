//! Test fixture: an app that uses `bulkhead:sockets` and `bulkhead:io` through
//! `bulkhead-sdk`'s `net` module, for the host's tests of app sockets. Its
//! UDP goes through the node (`bulkhead_native::app::Ctx::net`).
//!
//! Every socket the fixture holds has an id, which is also the tag of its
//! readiness notices. Bytes that arrive on a stream it watches are kept as
//! `<id>:<text>`; a stream the peer closed as `<id>:closed`. Connections a
//! watched listener accepts are watched in turn.
//!
//! Text commands; errors reply `err:<error>`:
//! - `connect <ip:port>`, `connect-host <host> <port>`: the stream's id.
//! - `listen <ip:port>`: `<id> <port>`; the listener is watched.
//! - `udp <ip:port>`: bind a UDP socket; its id.
//! - `send <id> <text>`: `ok`.
//! - `watch <id>`: watch the stream; `ok`.
//! - `close <id>`: drop the socket; `ok`.
//! - `received`: everything kept, joined by `,`.
//! - `open`: how many sockets the fixture holds.

use std::collections::BTreeMap;

use bulkhead_sdk::net::{self, NetError, TcpListener, TcpStream};
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Info, Reply};

enum Socket {
    Stream(TcpStream),
    Listener(TcpListener),
    // Held open to count against `max_sockets`; never read.
    Udp(#[allow(dead_code)] UdpSocket),
}

#[derive(Default)]
struct Fixture {
    sockets: BTreeMap<u64, Socket>,
    next: u64,
    received: Vec<String>,
}

impl Fixture {
    fn keep(&mut self, socket: Socket) -> u64 {
        self.next += 1;
        self.sockets.insert(self.next, socket);
        self.next
    }

    fn stream(&mut self, id: &str) -> Result<&mut TcpStream, NetError> {
        let id: u64 = id.parse().map_err(|_| NetError::Other("bad id".into()))?;
        match self.sockets.get_mut(&id) {
            Some(Socket::Stream(s)) => Ok(s),
            _ => Err(NetError::Other(format!("no stream {id}"))),
        }
    }

    fn command(&mut self, command: &str) -> Result<String, NetError> {
        let mut words = command.splitn(3, ' ');
        let verb = words.next().unwrap_or("");
        let first = words.next().unwrap_or("");
        let rest = words.next().unwrap_or("");
        let addr = || {
            first
                .parse()
                .map_err(|_| NetError::Other(format!("bad address {first}")))
        };
        Ok(match verb {
            "connect" => {
                let stream = TcpStream::connect(addr()?)?;
                self.keep(Socket::Stream(stream)).to_string()
            }
            "connect-host" => {
                let port = rest
                    .parse()
                    .map_err(|_| NetError::Other("bad port".into()))?;
                let stream = TcpStream::connect_host(first, port)?;
                self.keep(Socket::Stream(stream)).to_string()
            }
            "listen" => {
                let listener = TcpListener::bind(addr()?)?;
                let port = listener.local_addr()?.port();
                let id = self.keep(Socket::Listener(listener));
                if let Some(Socket::Listener(l)) = self.sockets.get_mut(&id) {
                    l.notify_acceptable(id)?;
                }
                format!("{id} {port}")
            }
            "udp" => {
                let socket = bind_udp(addr()?)?;
                self.keep(Socket::Udp(socket)).to_string()
            }
            "send" => {
                self.stream(first)?.write_all(rest.as_bytes())?;
                "ok".into()
            }
            "watch" => {
                let id = first.parse().unwrap_or(0);
                self.stream(first)?.notify_readable(id)?;
                "ok".into()
            }
            "close" => {
                self.sockets.remove(&first.parse().unwrap_or(0));
                "ok".into()
            }
            "received" => self.received.join(","),
            "open" => self.sockets.len().to_string(),
            other => return Err(NetError::Other(format!("unknown command {other}"))),
        })
    }

    /// A readiness notice for socket `id`.
    fn ready(&mut self, id: u64) {
        match self.sockets.get_mut(&id) {
            Some(Socket::Listener(listener)) => {
                let mut accepted = Vec::new();
                loop {
                    match listener.accept() {
                        Ok(Some(stream)) => accepted.push(stream),
                        Ok(None) => break,
                        Err(e) => {
                            self.received.push(format!("{id}:accept {e}"));
                            break;
                        }
                    }
                }
                let _ = listener.notify_acceptable(id);
                for stream in accepted {
                    let new = self.keep(Socket::Stream(stream));
                    if let Some(Socket::Stream(s)) = self.sockets.get_mut(&new) {
                        let _ = s.notify_readable(new);
                    }
                    self.received.push(format!("{id}:accepted {new}"));
                }
            }
            Some(Socket::Stream(stream)) => match stream.read(64 * 1024) {
                Ok(bytes) => {
                    if !bytes.is_empty() {
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        self.received.push(format!("{id}:{text}"));
                    }
                    let _ = stream.notify_readable(id);
                }
                Err(_) => {
                    self.received.push(format!("{id}:closed"));
                    self.sockets.remove(&id);
                }
            },
            _ => {}
        }
    }
}

/// A UDP socket the node holds, closed when dropped.
struct UdpSocket(u32);

impl Drop for UdpSocket {
    fn drop(&mut self) {
        bulkhead_native::app::close_socket_quietly(self.0);
    }
}

fn bind_udp(addr: std::net::SocketAddr) -> Result<UdpSocket, NetError> {
    use bulkhead_native::protocol::{NetAnswer, NetError as E, NetRequest};
    match bulkhead_native::app::Ctx::current().net(NetRequest::UdpBind(addr)) {
        Ok(NetAnswer::Udp { socket, .. }) => Ok(UdpSocket(socket)),
        Ok(other) => Err(NetError::Other(format!("unexpected {other:?}"))),
        Err(E::Denied) => Err(NetError::Denied),
        Err(E::Limited) => Err(NetError::Limited),
        Err(E::Closed) => Err(NetError::Closed),
        Err(E::Other(why)) => Err(NetError::Other(why)),
    }
}

impl App for Fixture {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self::default())
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let command = msg.as_text().unwrap_or("").to_string();
        let reply = self
            .command(&command)
            .unwrap_or_else(|e| format!("err:{e}"));
        Reply::Now(Envelope::text(reply))
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
        if let Some(id) = net::ready_tag(&info) {
            self.ready(id);
        }
    }
}

bulkhead_sdk::app!(Fixture);
