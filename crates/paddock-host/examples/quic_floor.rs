//! The floor under a call between nodes: bare quinn, without Paddock, so
//! `cluster_throughput`'s two-node call latency can be split into what
//! QUIC and tokio cost and what Paddock adds. TLS 1.3 on ring (a
//! self-signed certificate; Paddock pins raw keys, which costs the same per
//! packet), one bidirectional stream, length-prefixed frames of 64 bytes,
//! one request in flight: the client writes a frame, the server reads it
//! and writes it back. A bare UDP echo of the same 64 bytes gives the
//! floor under QUIC.
//!
//! ```sh
//! cargo run --release -p paddock-host --example quic_floor -- 4
//! # Between two machines: the server on one, the client on the other.
//! cargo run --release -p paddock-host --example quic_floor -- 4 serve 0.0.0.0:4433
//! cargo run --release -p paddock-host --example quic_floor -- 4 connect HOST:4433
//! ```
//!
//! Arguments: tokio worker threads (default 4), then nothing for both ends
//! in one process over loopback, `serve ADDR:PORT` (QUIC on that port, the
//! UDP echo on the next; runs until killed), or `connect HOST:PORT` for
//! the client of a server elsewhere. The client prints one line per
//! variant:
//!
//! - `two writes`: length, then body, as Paddock's `write_frame` does.
//! - `one write`: length and body in one buffer.
//! - `current thread`: one write, the client on a single-threaded runtime,
//!   so no wake crosses threads on its side (in one process, on neither).
//! - `udp`: a datagram out and back on a connected socket; a reply not
//!   back within a second counts as lost.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Endpoint, RecvStream, SendStream};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring::default_provider;
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::{DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use tokio::net::UdpSocket;

const FRAME: usize = 64;
const WARMUP: usize = 2_000;
const TIMED: Duration = Duration::from_secs(5);
const UDP_LOST: Duration = Duration::from_secs(1);

/// Where the server is.
#[derive(Clone, Copy)]
enum Target {
    /// In this process, started per run.
    Local,
    /// Elsewhere: QUIC at this address, the UDP echo on the next port.
    Remote(SocketAddr),
}

fn main() {
    let mut args = std::env::args().skip(1);
    let workers: usize = args.next().map_or(4, |a| a.parse().expect("workers"));
    let target = match (args.next().as_deref(), args.next()) {
        (None, _) => Target::Local,
        (Some("serve"), Some(address)) => {
            let address: SocketAddr = address.parse().expect("ADDR:PORT");
            println!(
                "quic_floor: serving QUIC on {address}, UDP echo on port {}",
                address.port() + 1
            );
            runtime(workers).block_on(serve(address));
            return;
        }
        (Some("connect"), Some(address)) => Target::Remote(resolve(&address)),
        _ => panic!("usage: quic_floor [WORKERS] [serve ADDR:PORT | connect HOST:PORT]"),
    };
    let over = match target {
        Target::Local => "over loopback".to_string(),
        Target::Remote(address) => format!("to {address}"),
    };
    println!("quic_floor: bare quinn ping-pong {over}, {FRAME}-byte frames, one in flight");
    let multi = runtime(workers);
    for (name, one_write) in [("two writes", false), ("one write", true)] {
        let line = multi.block_on(quic(target, one_write));
        println!("{workers} workers, {name}: {line}");
    }
    let single = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let line = single.block_on(quic(target, true));
    println!("current thread, one write: {line}");
    let line = multi.block_on(udp(target));
    println!("{workers} workers, udp: {line}");
}

fn runtime(workers: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
}

/// The first IPv4 address of `HOST:PORT`, else the first.
fn resolve(address: &str) -> SocketAddr {
    let all: Vec<SocketAddr> = address.to_socket_addrs().expect("HOST:PORT").collect();
    *all.iter()
        .find(|a| a.is_ipv4())
        .or(all.first())
        .expect("no address")
}

/// The server side, until killed: QUIC echo at `address`, UDP echo on the
/// next port.
async fn serve(address: SocketAddr) {
    let mut udp_address = address;
    udp_address.set_port(address.port() + 1);
    tokio::spawn(udp_echo(UdpSocket::bind(udp_address).await.unwrap()));
    quic_echo(server_endpoint(address)).await;
}

/// Echo frames on every stream of every connection.
async fn quic_echo(server: Endpoint) {
    while let Some(incoming) = server.accept().await {
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            while let Ok((send, recv)) = conn.accept_bi().await {
                tokio::spawn(echo(send, recv));
            }
        });
    }
}

/// One stream: a byte saying whether to reply in one write, then frames.
async fn echo(mut send: SendStream, mut recv: RecvStream) {
    let mut one_write = [0u8; 1];
    if recv.read_exact(&mut one_write).await.is_err() {
        return;
    }
    let mut buf = vec![0u8; FRAME];
    while read_frame(&mut recv, &mut buf).await {
        write_frame(&mut send, &buf, one_write[0] == 1).await;
    }
}

async fn udp_echo(socket: UdpSocket) {
    let mut buf = [0u8; 2048];
    while let Ok((n, from)) = socket.recv_from(&mut buf).await {
        let _ = socket.send_to(&buf[..n], from).await;
    }
}

/// One timed QUIC ping-pong run; its summary line.
async fn quic(target: Target, one_write: bool) -> String {
    let (client, address, serving) = match target {
        Target::Local => {
            let server = server_endpoint("127.0.0.1:0".parse().unwrap());
            let address = server.local_addr().unwrap();
            let serving = tokio::spawn(quic_echo(server));
            (client_endpoint("127.0.0.1:0"), address, Some(serving))
        }
        Target::Remote(address) => (client_endpoint("0.0.0.0:0"), address, None),
    };
    let conn = client.connect(address, "localhost").unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&[u8::from(one_write)]).await.unwrap();
    let ping = vec![7u8; FRAME];
    let mut buf = vec![0u8; FRAME];
    let line = timed(async || {
        write_frame(&mut send, &ping, one_write).await;
        assert!(read_frame(&mut recv, &mut buf).await);
        true
    })
    .await;
    send.finish().unwrap();
    conn.close(0u32.into(), b"done");
    client.wait_idle().await;
    if let Some(serving) = serving {
        serving.abort();
    }
    line
}

/// One timed UDP ping-pong run; its summary line.
async fn udp(target: Target) -> String {
    let (address, serving) = match target {
        Target::Local => {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = socket.local_addr().unwrap();
            (address, Some(tokio::spawn(udp_echo(socket))))
        }
        Target::Remote(mut address) => {
            address.set_port(address.port() + 1);
            (address, None)
        }
    };
    let bind = if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).await.unwrap();
    socket.connect(address).await.unwrap();
    let ping = [7u8; FRAME];
    let mut buf = [0u8; 2048];
    let line = timed(async || {
        socket.send(&ping).await.unwrap();
        matches!(
            tokio::time::timeout(UDP_LOST, socket.recv(&mut buf)).await,
            Ok(Ok(FRAME))
        )
    })
    .await;
    if let Some(serving) = serving {
        serving.abort();
    }
    line
}

/// Warm up, then time round trips for `TIMED`; `round_trip` returns false
/// for one that was lost, which is counted apart.
async fn timed(mut round_trip: impl AsyncFnMut() -> bool) -> String {
    for _ in 0..WARMUP {
        round_trip().await;
    }
    let mut us = Vec::new();
    let mut lost = 0;
    let started = Instant::now();
    while started.elapsed() < TIMED {
        let t = Instant::now();
        if round_trip().await {
            us.push(t.elapsed().as_secs_f64() * 1e6);
        } else {
            lost += 1;
        }
    }
    us.sort_by(f64::total_cmp);
    let lost = if lost > 0 {
        format!(", {lost} lost")
    } else {
        String::new()
    };
    format!(
        "{:.0} round trips/s, p50 {:.1} us, p99 {:.1} us{lost}",
        us.len() as f64 / TIMED.as_secs_f64(),
        us[us.len() / 2],
        us[(us.len() - 1) * 99 / 100]
    )
}

async fn write_frame(send: &mut SendStream, body: &[u8], one_write: bool) {
    let len = u32::try_from(body.len()).unwrap().to_be_bytes();
    if one_write {
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&len);
        frame.extend_from_slice(body);
        send.write_all(&frame).await.unwrap();
    } else {
        send.write_all(&len).await.unwrap();
        send.write_all(body).await.unwrap();
    }
}

/// Read one frame into `buf`; false once the stream finished.
async fn read_frame(recv: &mut RecvStream, buf: &mut [u8]) -> bool {
    let mut len = [0u8; 4];
    if recv.read_exact(&mut len).await.is_err() {
        return false;
    }
    assert_eq!(u32::from_be_bytes(len) as usize, buf.len());
    recv.read_exact(buf).await.is_ok()
}

/// A server endpoint at `address` with a self-signed certificate for
/// `localhost`.
fn server_endpoint(address: SocketAddr) -> Endpoint {
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = CertificateDer::from(made.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(made.signing_key.serialize_der()));
    let mut server_tls = rustls::ServerConfig::builder_with_provider(Arc::new(default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    server_tls.alpn_protocols = vec![b"floor".to_vec()];
    let server_config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_tls).unwrap()));
    Endpoint::server(server_config, address).unwrap()
}

/// A client endpoint bound to `bind` that takes any server certificate but
/// checks the handshake signature, as Paddock's pinned keys do.
fn client_endpoint(bind: &str) -> Endpoint {
    let provider = Arc::new(default_provider());
    let mut client_tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(
            provider.signature_verification_algorithms,
        )))
        .with_no_client_auth();
    client_tls.alpn_protocols = vec![b"floor".to_vec()];
    let mut client = Endpoint::client(bind.parse().unwrap()).unwrap();
    client.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(client_tls).unwrap(),
    )));
    client
}

/// Any certificate is the server's; its handshake signature is still
/// checked, so the handshake costs what a verified one does.
#[derive(Debug)]
struct AnyCertificate(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}
