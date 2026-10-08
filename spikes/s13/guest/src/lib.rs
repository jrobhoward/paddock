//! S13 guest: serves HTTP/1.1 on a connection the host accepted, read and
//! written through `wasi:io` streams. `GET /crash` panics (a trap).

mod bindings {
    wit_bindgen::generate!({
        path: "../wit",
        world: "s13:conn/conn",
        generate_all,
    });
}

use bindings::exports::s13::conn::handler::Guest;
use bindings::wasi::io::streams::{InputStream, OutputStream};

#[cfg(not(feature = "v2"))]
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
#[cfg(feature = "v2")]
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\nok2";

struct Handler;

impl Guest for Handler {
    fn serve(input: InputStream, output: OutputStream) {
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        loop {
            while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                // The request line may carry an absolute URL; look for the path.
                let line_end = buf.iter().position(|b| *b == b'\r').unwrap_or(end);
                let line = &buf[..line_end];
                assert!(
                    !line.windows(7).any(|w| w == b"/crash "),
                    "asked to crash"
                );
                buf.drain(..end + 4);
                if output.blocking_write_and_flush(RESPONSE).is_err() {
                    return;
                }
            }
            match input.blocking_read(4096) {
                Ok(bytes) if !bytes.is_empty() => buf.extend_from_slice(&bytes),
                Ok(_) => {}
                Err(_) => return, // closed
            }
        }
    }
}

bindings::export!(Handler with_types_in bindings);
