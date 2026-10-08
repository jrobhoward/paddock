//! S15: an HTTP client inside the guest, timed inside the guest.
//!
//! Usage: s15-client <http|tcp> <host:port> <n>
//! - `http`: `n` GETs through `wasi:http/outgoing-handler` (wstd's Client).
//! - `tcp`: `n` GETs over one keep-alive connection through `wasi:sockets`,
//!   parsing responses itself.
//!
//! Prints the total nanoseconds.

use std::time::Instant;

use wstd::http::{Body, Client, Request};
use wstd::io::{self, AsyncRead, AsyncWrite};
use wstd::net::TcpStream;

async fn over_http(addr: &str, n: u32) -> u128 {
    let client = Client::new();
    let started = Instant::now();
    for _ in 0..n {
        let request = Request::get(format!("http://{addr}/")).body(Body::empty()).unwrap();
        let response = client.send(request).await.unwrap();
        let mut body = response.into_body();
        assert_eq!(body.contents().await.unwrap(), b"ok");
    }
    started.elapsed().as_nanos()
}

async fn over_tcp(addr: &str, n: u32) -> io::Result<u128> {
    let stream = TcpStream::connect(addr).await?;
    let request = format!("GET / HTTP/1.1\r\nhost: {addr}\r\n\r\n");
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let started = Instant::now();
    for _ in 0..n {
        (&stream).write_all(request.as_bytes()).await?;
        // Read headers, then content-length bytes of body.
        let (head_end, len) = loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().unwrap_or(0));
                break (end + 4, len);
            }
            let got = (&stream).read(&mut chunk).await?;
            assert!(got > 0, "connection closed");
            buf.extend_from_slice(&chunk[..got]);
        };
        while buf.len() < head_end + len {
            let got = (&stream).read(&mut chunk).await?;
            assert!(got > 0, "connection closed");
            buf.extend_from_slice(&chunk[..got]);
        }
        assert_eq!(&buf[head_end..head_end + len], b"ok");
        buf.drain(..head_end + len);
    }
    Ok(started.elapsed().as_nanos())
}

#[wstd::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (mode, addr, n) = (&args[1], &args[2], args[3].parse::<u32>().unwrap());
    let ns = if mode == "http" {
        over_http(addr, n).await
    } else {
        over_tcp(addr, n).await?
    };
    println!("{ns}");
    Ok(())
}
