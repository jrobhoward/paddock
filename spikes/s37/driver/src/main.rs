//! S37's ingress driver: a connection handler (the node accepts on its
//! listeners and passes each connection, reading none of it) that parses
//! HTTP/1.1 itself, terminates TLS itself on the `https` listener, and
//! makes each request a call through the node to an instance of the app,
//! found by its registered name `echo-<n>`, in turn.
//!
//! The certificate is made at the first TLS connection (rcgen, P-256, as
//! the node's ingress in the harness): a spike's, so the client trusts
//! any.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bulkhead_sdk::connection::{ConnectionInfo, Flow, Handler, Output};
use bulkhead_sdk::http::{Request, Response};
use bulkhead_sdk::{Ctx, Pid};

/// The largest request body the driver takes.
const MAX_BODY: usize = 4 << 20;
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

struct Conn {
    tls: Option<rustls::ServerConnection>,
    scheme: &'static str,
    buf: Vec<u8>,
}

/// The app's instances, as found by name, and the next to call.
static TARGETS: Mutex<(Vec<Pid>, usize)> = Mutex::new((Vec::new(), 0));

fn target(ctx: &mut Ctx) -> Option<Pid> {
    let mut t = TARGETS.lock().unwrap();
    if t.0.is_empty() {
        t.0 = (0..64).map_while(|n| ctx.whereis(&format!("echo-{n}"))).collect();
    }
    if t.0.is_empty() {
        return None;
    }
    t.1 = (t.1 + 1) % t.0.len();
    Some(t.0[t.1])
}

fn forget_targets() {
    TARGETS.lock().unwrap().0.clear();
}

fn tls_config() -> Arc<rustls::ServerConfig> {
    static CONFIG: OnceLock<Arc<rustls::ServerConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
            let cert = made.cert.der().clone();
            let key = rustls::pki_types::PrivateKeyDer::try_from(made.signing_key.serialize_der()).unwrap();
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
            Arc::new(config)
        })
        .clone()
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
    let request = Request {
        scheme: scheme.to_string(),
        method: req.method.unwrap_or("GET").to_string(),
        path,
        query,
        headers: out,
        body: buf[head..head + length].to_vec(),
        key: None,
    };
    Ok(Some((request, head + length)))
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

impl Conn {
    /// Serve every whole request in `buf`; the response bytes to send.
    fn serve(&mut self, ctx: &mut Ctx) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        while let Some((req, used)) = parse(&self.buf, self.scheme)? {
            self.buf.drain(..used);
            // `/local`: answered here, with no call, to split the cost.
            let resp = if req.path == "/local" {
                Response::new(200).body(b"ok".to_vec())
            } else { match target(ctx) {
                None => Response::text(503, "no app"),
                Some(pid) => match ctx.call(pid, req.to_envelope(), CALL_TIMEOUT) {
                    Ok(reply) => Response::from_envelope(&reply)
                        .unwrap_or_else(|| Response::text(502, "not a response")),
                    Err(e) => {
                        forget_targets();
                        Response::text(502, e.to_string())
                    }
                },
            } };
            out.extend(respond(&resp));
        }
        Ok(out)
    }
}

impl Handler for Conn {
    fn open(_ctx: &mut Ctx, info: &ConnectionInfo) -> Result<Self, String> {
        let tls = (info.listener == "https")
            .then(|| rustls::ServerConnection::new(tls_config()))
            .transpose()
            .map_err(|e| e.to_string())?;
        Ok(Conn {
            scheme: if tls.is_some() { "https" } else { "http" },
            tls,
            buf: Vec::new(),
        })
    }

    fn data(&mut self, ctx: &mut Ctx, bytes: &[u8], out: &mut Output<'_>) -> Result<Flow, String> {
        let Some(tls) = &mut self.tls else {
            self.buf.extend_from_slice(bytes);
            let reply = self.serve(ctx)?;
            out.write_all(&reply).map_err(|e| e.to_string())?;
            return Ok(Flow::Continue);
        };
        let mut input = bytes;
        while !input.is_empty() {
            tls.read_tls(&mut input).map_err(|e| e.to_string())?;
            let state = tls.process_new_packets().map_err(|e| e.to_string())?;
            let mut plain = vec![0u8; state.plaintext_bytes_to_read()];
            tls.reader().read_exact(&mut plain).map_err(|e| e.to_string())?;
            self.buf.extend_from_slice(&plain);
        }
        let reply = self.serve(ctx)?;
        let tls = self.tls.as_mut().unwrap();
        if !reply.is_empty() {
            tls.writer().write_all(&reply).map_err(|e| e.to_string())?;
        }
        while tls.wants_write() {
            tls.write_tls(out).map_err(|e| e.to_string())?;
        }
        Ok(Flow::Continue)
    }
}

bulkhead_sdk::connection_handler!(Conn);

fn main() -> std::process::ExitCode {
    bulkhead_main()
}
