//! The floor under a call between nodes: bare quinn over
//! loopback, without Paddock, so `cluster_throughput`'s two-node call
//! latency can be split into what QUIC and tokio cost and what Paddock
//! adds. Two endpoints in one process, TLS 1.3 on ring (a self-signed
//! certificate; Paddock pins raw keys, which costs the same per packet),
//! one bidirectional stream, length-prefixed frames of 64 bytes, one
//! request in flight: the client writes a frame, the server reads it and
//! writes it back.
//!
//! ```sh
//! cargo run --release -p paddock-host --example quic_floor -- 4
//! ```
//!
//! Argument: tokio worker threads (default 4). Each line is one variant:
//!
//! - `two writes`: length, then body, as Paddock's `write_frame` does.
//! - `one write`: length and body in one buffer.
//! - `current thread`: one write, on a single-threaded runtime, so no wake
//!   crosses threads.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Endpoint, RecvStream, SendStream};
use rustls::crypto::ring::default_provider;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const FRAME: usize = 64;
const WARMUP: usize = 2_000;
const TIMED: Duration = Duration::from_secs(5);

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |a| a.parse().expect("workers"));
    println!("quic_floor: bare quinn ping-pong over loopback, {FRAME}-byte frames, one in flight");
    let multi = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap();
    for (name, one_write) in [("two writes", false), ("one write", true)] {
        let line = multi.block_on(run(one_write));
        println!("{workers} workers, {name}: {line}");
    }
    let single = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let line = single.block_on(run(true));
    println!("current thread, one write: {line}");
}

/// One timed ping-pong run; its summary line.
async fn run(one_write: bool) -> String {
    let (server, client, address) = endpoints();
    let serving = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let mut buf = vec![0u8; FRAME];
        while read_frame(&mut recv, &mut buf).await {
            write_frame(&mut send, &buf, one_write).await;
        }
    });
    let conn = client.connect(address, "localhost").unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let ping = vec![7u8; FRAME];
    let mut buf = vec![0u8; FRAME];
    for _ in 0..WARMUP {
        write_frame(&mut send, &ping, one_write).await;
        assert!(read_frame(&mut recv, &mut buf).await);
    }
    let mut us = Vec::new();
    let started = Instant::now();
    while started.elapsed() < TIMED {
        let t = Instant::now();
        write_frame(&mut send, &ping, one_write).await;
        assert!(read_frame(&mut recv, &mut buf).await);
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    send.finish().unwrap();
    let _ = serving.await;
    us.sort_by(f64::total_cmp);
    format!(
        "{:.0} round trips/s, p50 {:.1} us, p99 {:.1} us",
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

/// A server endpoint with a self-signed certificate for `localhost`, a
/// client endpoint trusting it, and the server's address.
fn endpoints() -> (Endpoint, Endpoint, SocketAddr) {
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = CertificateDer::from(made.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(made.signing_key.serialize_der()));
    let provider = Arc::new(default_provider());

    let mut server_tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .unwrap();
    server_tls.alpn_protocols = vec![b"floor".to_vec()];
    let server_config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_tls).unwrap()));
    let server = Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let address = server.local_addr().unwrap();

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let mut client_tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client_tls.alpn_protocols = vec![b"floor".to_vec()];
    let mut client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(client_tls).unwrap(),
    )));
    (server, client, address)
}
