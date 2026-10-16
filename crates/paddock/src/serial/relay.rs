//! A serial line between two ttys that can be cut: for tests and the soak,
//! which join two nodes' devices through it (two ptys, or a pty and a real
//! device) and cut the line on a fault schedule, as UDP proxies cut the
//! network.

use std::time::Duration;

use tokio::sync::watch;

use super::tty::Tty;

/// Copy bytes both ways between `a` and `b`, dropping them while `cut`
/// holds `true`, until the returned future is dropped. A read that fails
/// (a pty whose other end nobody has opened yet, or has closed) is retried
/// after 20 ms, so either node can start, stop and restart.
pub async fn relay(a: Tty, b: Tty, cut: watch::Receiver<bool>) {
    tokio::join!(one_way(&a, &b, cut.clone()), one_way(&b, &a, cut),);
}

async fn one_way(from: &Tty, to: &Tty, cut: watch::Receiver<bool>) {
    let mut buf = [0u8; 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(n) if n > 0 => n,
            _ => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
        };
        if !*cut.borrow() {
            // The far end may be closed too; the bytes are lost, as on a
            // cable to a node that is down.
            let _ = to.write_all(&buf[..n]).await;
        }
    }
}
