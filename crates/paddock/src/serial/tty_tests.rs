#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use super::*;

/// A pty whose other end was opened and closed once (tokio has seen the
/// hang-up), then opened again: the controlling end, and the open end.
async fn reopened() -> (Arc<Tty>, Tty) {
    let (master, path) = pty().unwrap();
    drop(Tty::open(&path, 115_200).unwrap());
    let mut buf = [0u8; 16];
    // Seeing the hang-up marks the descriptor closed for good.
    let _ = tokio::time::timeout(Duration::from_secs(1), master.read(&mut buf)).await;
    let again = Tty::open(&path, 115_200).unwrap();
    (Arc::new(master), again)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read____after_a_hang_up_with_nothing_to_read____returns_an_error_instead_of_spinning() {
    let (master, _open) = reopened().await;

    let reader = tokio::spawn(async move { master.read(&mut [0u8; 16]).await });
    let result = tokio::time::timeout(Duration::from_secs(2), reader).await;

    let read = result.expect("read returned instead of spinning").unwrap();
    assert_eq!(read.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read____after_a_hang_up_then_data____reads_it() {
    let (master, open) = reopened().await;

    open.write_all(b"beat").await.unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 16];
    for _ in 0..50 {
        match master.read(&mut buf).await {
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
        if got.len() >= 4 {
            break;
        }
    }

    assert_eq!(got, b"beat");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_all____after_a_hang_up_with_a_full_buffer____returns_an_error_instead_of_spinning() {
    // Nobody reads the open end, so the pty's buffer fills.
    let (master, _open) = reopened().await;

    let writer = tokio::spawn(async move { master.write_all(&vec![b'x'; 1 << 20]).await });
    let result = tokio::time::timeout(Duration::from_secs(2), writer).await;

    let written = result.expect("write returned instead of spinning").unwrap();
    assert!(written.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open____not_a_device____error() {
    let result = Tty::open(std::path::Path::new("/dev/paddock-no-such-tty"), 115_200);

    assert!(result.is_err());
}
