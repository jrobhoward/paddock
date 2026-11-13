//! DA1's ingress driver, for S37's harness: the same HTTP/1.1 and TLS as
//! S37's driver, on what DA1 built instead of a connection handler.
//!
//! - It accepts its own connections on the listening sockets node config
//!   `[drivers.driver]` binds and passes (`bulkhead_native::driver`); the
//!   node sees none of their bytes.
//! - `threads` worker threads (init arguments, default 4) each hold a
//!   sender channel, poll both listeners and their own connections, and
//!   make each request a call to the app `echo` by name
//!   (`start_call_app`), with any number in flight, answered as they come.
//! - TLS from the certificate and key the profile gives it under /driver.
//!
//! `/local` is answered by the driver itself, with no call, to split the
//! cost as S37 did.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;
use std::time::Duration;

use bulkhead_sdk::http::{Request, Response};
use bulkhead_sdk::sender::Sender;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};

const MAX_BODY: usize = 4 << 20;
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

struct Driver;

fn nonblocking(fd: impl AsFd) {
    let fd = fd.as_fd();
    // SAFETY: fcntl on a descriptor this process holds.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

fn tls_config() -> Result<Arc<rustls::ServerConfig>, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let cert = bulkhead_sdk::files::path("/driver/c.pem");
    let key = bulkhead_sdk::files::path("/driver/k.pem");
    let certs = CertificateDer::pem_file_iter(&cert)
        .map_err(|e| format!("{}: {e}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let key = PrivateKeyDer::from_pem_file(&key).map_err(|e| format!("{}: {e}", key.display()))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| e.to_string())?;
    Ok(Arc::new(config))
}

impl App for Driver {
    fn init(ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let args = serde_json::from_slice::<serde_json::Value>(args).unwrap_or_default();
        let threads = args["threads"].as_u64().unwrap_or(4) as usize;
        let _ = SPIN.set(Duration::from_micros(args["spin_us"].as_u64().unwrap_or(0)));
        let listeners: Vec<(String, TcpListener)> = bulkhead_native::driver::listeners()
            .into_iter()
            .map(|(name, fd)| {
                let l = TcpListener::from(fd);
                nonblocking(&l);
                (name, l)
            })
            .collect();
        if listeners.is_empty() {
            return Err("no listeners: not configured as a driver".into());
        }
        let listeners = Arc::new(listeners);
        let tls = tls_config()?;
        for _ in 0..threads {
            let sender = ctx.sender().map_err(|e| e.to_string())?;
            let (l, t) = (listeners.clone(), tls.clone());
            std::thread::spawn(move || worker(sender, &l, &t));
        }
        Ok(Driver)
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, _msg: Envelope) -> Reply {
        Reply::Now(Envelope::text("a driver"))
    }
}

struct Conn {
    stream: TcpStream,
    tls: Option<rustls::ServerConnection>,
    scheme: &'static str,
    buf: Vec<u8>,
    /// A request of this connection's is in flight.
    busy: bool,
    /// The peer closed, or the connection broke: drop once idle.
    closed: bool,
}

/// Writes all of what it is given to a non-blocking stream, waiting for
/// room when it is full.
struct Blocking<'a>(&'a TcpStream);

impl Write for Blocking<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            match (&*self.0).write(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let mut p = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                    // SAFETY: one valid pollfd.
                    unsafe { libc::poll(&mut p, 1, 1000) };
                }
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Conn {
    /// Read what the peer sent, through TLS if any; `false` at its end or
    /// an error.
    fn read(&mut self) -> bool {
        let mut chunk = [0u8; 16 << 10];
        loop {
            let n = match &mut self.tls {
                None => match (&self.stream).read(&mut chunk) {
                    Ok(0) => return false,
                    Ok(n) => {
                        self.buf.extend_from_slice(&chunk[..n]);
                        n
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                    Err(_) => return false,
                },
                Some(tls) => match tls.read_tls(&mut &self.stream) {
                    Ok(0) => return false,
                    Ok(n) => {
                        if tls.process_new_packets().is_err() {
                            return false;
                        }
                        let mut plain = Vec::new();
                        let _ = tls.reader().read_to_end(&mut plain);
                        self.buf.extend_from_slice(&plain);
                        while tls.wants_write() {
                            if tls.write_tls(&mut Blocking(&self.stream)).is_err() {
                                return false;
                            }
                        }
                        n
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                    Err(_) => return false,
                },
            };
            let _ = n;
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let ok = match &mut self.tls {
            None => Blocking(&self.stream).write_all(bytes).is_ok(),
            Some(tls) => {
                let mut ok = tls.writer().write_all(bytes).is_ok();
                while ok && tls.wants_write() {
                    ok = tls.write_tls(&mut Blocking(&self.stream)).is_ok();
                }
                ok
            }
        };
        if !ok {
            self.closed = true;
        }
    }
}

/// One whole request at the start of `buf`: it and its length.
fn parse(buf: &[u8], scheme: &str) -> Result<Option<(Request, usize)>, String> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let head = match req.parse(buf).map_err(|e| e.to_string())? {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => return Ok(None),
    };
    let mut length = 0usize;
    let mut out = Vec::with_capacity(req.headers.len());
    for h in req.headers.iter() {
        let name = h.name.to_ascii_lowercase();
        let value = String::from_utf8_lossy(h.value).into_owned();
        if name == "content-length" {
            length = value.trim().parse().map_err(|_| "bad content-length")?;
        }
        out.push((name, value));
    }
    if length > MAX_BODY {
        return Err("body too large".into());
    }
    if buf.len() < head + length {
        return Ok(None);
    }
    let target = req.path.unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (target.to_string(), None),
    };
    Ok(Some((
        Request {
            scheme: scheme.to_string(),
            method: req.method.unwrap_or("GET").to_string(),
            path,
            query,
            headers: out,
            body: buf[head..head + length].to_vec(),
            key: None,
        },
        head + length,
    )))
}

fn respond(resp: &Response) -> Vec<u8> {
    let mut out = Vec::with_capacity(resp.body.len() + 128);
    let _ = write!(out, "HTTP/1.1 {} OK\r\n", resp.status);
    for (k, v) in &resp.headers {
        let _ = write!(out, "{k}: {v}\r\n");
    }
    let _ = write!(out, "content-length: {}\r\n\r\n", resp.body.len());
    out.extend_from_slice(&resp.body);
    out
}

/// Start the next request buffered on `conns[i]`, if it is idle and one
/// is whole: a call in flight, or `/local` answered here.
fn next_request(sender: &mut Sender, conns: &mut [Option<Conn>], i: usize, pending: &mut HashMap<u64, usize>) {
    loop {
        let Some(c) = conns[i].as_mut() else { return };
        if c.busy || c.closed {
            return;
        }
        match parse(&c.buf, c.scheme) {
            Ok(None) => return,
            Err(_) => {
                c.send(&respond(&Response::text(400, "bad request")));
                c.closed = true;
                return;
            }
            Ok(Some((req, used))) => {
                c.buf.drain(..used);
                if req.path == "/local" {
                    c.send(&respond(&Response::new(200).body(b"ok".to_vec())));
                    continue;
                }
                let id = sender.start_call_app("echo", req.to_envelope(), CALL_TIMEOUT);
                pending.insert(id, i);
                c.busy = true;
                return;
            }
        }
    }
}

fn worker(mut sender: Sender, listeners: &[(String, TcpListener)], tls: &Arc<rustls::ServerConfig>) {
    let mut conns: Vec<Option<Conn>> = Vec::new();
    let mut pending: HashMap<u64, usize> = HashMap::new();
    let mut fds: Vec<libc::pollfd> = Vec::new();
    let mut index: Vec<usize> = Vec::new();
    loop {
        // With calls in flight, look at the sockets without waiting, then
        // take the next answer; otherwise wait on the sockets.
        let busy = sender.in_flight() > 0;
        // Idle: poll without waiting for a while before sleeping in `poll`,
        // as the rings spin before they sleep (`spin_us`).
        let ready = busy && poll_ready(listeners, &conns, &mut fds, &mut index, 0)
            || !busy && spin_then_poll(listeners, &conns, &mut fds, &mut index);
        if ready {
            handle_events(&mut sender, listeners, tls, &mut conns, &mut pending, &fds, &index);
        }
        if busy && let Some((id, result)) = sender.next_answer() {
            if let Some(i) = pending.remove(&id) {
                if let Some(c) = conns[i].as_mut() {
                    let resp = match result {
                        Ok(reply) => Response::from_envelope(&reply)
                            .unwrap_or_else(|| Response::text(502, "not a response")),
                        Err(e) => Response::text(502, e.to_string()),
                    };
                    c.send(&respond(&resp));
                    c.busy = false;
                }
                next_request(&mut sender, &mut conns, i, &mut pending);
            }
        }
        for slot in conns.iter_mut() {
            if slot.as_ref().is_some_and(|c| c.closed && !c.busy) {
                *slot = None;
            }
        }
    }
}

fn handle_events(
    sender: &mut Sender,
    listeners: &[(String, TcpListener)],
    tls: &Arc<rustls::ServerConfig>,
    conns: &mut Vec<Option<Conn>>,
    pending: &mut HashMap<u64, usize>,
    fds: &[libc::pollfd],
    index: &[usize],
) {
    for (k, p) in fds.iter().enumerate() {
        if p.revents == 0 {
            continue;
        }
        if k < listeners.len() {
            let (name, l) = &listeners[k];
            while let Ok((stream, _)) = l.accept() {
                nonblocking(&stream);
                let _ = stream.set_nodelay(true);
                let tls = (name == "https").then(|| rustls::ServerConnection::new(tls.clone()).unwrap());
                let conn = Conn {
                    stream,
                    scheme: if tls.is_some() { "https" } else { "http" },
                    tls,
                    buf: Vec::new(),
                    busy: false,
                    closed: false,
                };
                match conns.iter().position(Option::is_none) {
                    Some(free) => conns[free] = Some(conn),
                    None => conns.push(Some(conn)),
                }
            }
        } else {
            let i = index[k - listeners.len()];
            if let Some(c) = conns[i].as_mut()
                && !c.read()
            {
                c.closed = true;
            }
            next_request(sender, conns, i, pending);
        }
    }
}

/// How long an idle worker polls without waiting before it sleeps (init
/// argument `spin_us`, default 0).
static SPIN: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

/// Poll without waiting for up to [`SPIN`], yielding between polls, then
/// wait.
fn spin_then_poll(
    listeners: &[(String, TcpListener)],
    conns: &[Option<Conn>],
    fds: &mut Vec<libc::pollfd>,
    index: &mut Vec<usize>,
) -> bool {
    let spin = *SPIN.get().unwrap_or(&Duration::ZERO);
    let start = std::time::Instant::now();
    while start.elapsed() < spin {
        if poll_ready(listeners, conns, fds, index, 0) {
            return true;
        }
        std::thread::yield_now();
    }
    poll_ready(listeners, conns, fds, index, -1)
}

/// Poll the listeners and the idle open connections; whether any is ready.
fn poll_ready(
    listeners: &[(String, TcpListener)],
    conns: &[Option<Conn>],
    fds: &mut Vec<libc::pollfd>,
    index: &mut Vec<usize>,
    timeout: i32,
) -> bool {
    fds.clear();
    index.clear();
    for (_, l) in listeners {
        fds.push(libc::pollfd { fd: l.as_raw_fd(), events: libc::POLLIN, revents: 0 });
    }
    for (i, c) in conns.iter().enumerate() {
        if let Some(c) = c
            && !c.busy
            && !c.closed
        {
            fds.push(libc::pollfd { fd: c.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            index.push(i);
        }
    }
    // SAFETY: a valid buffer of fds.len() pollfds.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
    n > 0
}

bulkhead_sdk::app!(Driver);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
