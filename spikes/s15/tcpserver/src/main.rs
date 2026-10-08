//! S15: an HTTP/1.1 server that owns its socket (`wasi:sockets`) and parses
//! requests itself. Answers every GET with `ok`, keeps connections alive.
//!
//! Usage: s15-tcpserver <addr>

use wstd::io::{self, AsyncRead, AsyncWrite};
use wstd::iter::AsyncIterator;
use wstd::net::{TcpListener, TcpStream};

const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

async fn serve(stream: TcpStream) -> io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        // Requests have no body: each ends at a blank line.
        while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            buf.drain(..end + 4);
            (&stream).write_all(RESPONSE).await?;
        }
        let n = (&stream).read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[wstd::main]
async fn main() -> io::Result<()> {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:0".into());
    let listener = TcpListener::bind(&addr).await?;
    println!("{}", listener.local_addr()?);
    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        wstd::runtime::spawn(async move {
            let _ = serve(stream).await;
        })
        .detach();
    }
    Ok(())
}
