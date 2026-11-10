//! Free ports for the nodes a test or a soak starts, which bind them
//! themselves.
//!
//! A port found by binding port 0 comes from the kernel's ephemeral range,
//! which it also hands to every outgoing connection and every other bind of
//! port 0; with many tests running, one of those could take the port between
//! its pick and the node's bind (`Address already in use`, seen in the
//! `board` tests and the `scada` soak, 2026-11-09). These ports come from
//! below that range instead, where only an explicit bind lands, and each is
//! held by a lock file for the life of the process, so test binaries and
//! soak runs at the same time never pick the same one.
//!
//! Shared by path: `bulkhead`'s tests and `bulkhead-soak` include this file.

use std::fs::{self, File, TryLockError};
use std::io;
use std::net::{TcpListener, UdpSocket};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// The lock files of the ports this process picked.
static HELD: Mutex<Vec<File>> = Mutex::new(Vec::new());

/// How many ports below the ephemeral range to choose from.
const SPAN: u16 = 12_000;

/// A port on 127.0.0.1 that is free for both TCP and UDP, below the
/// kernel's ephemeral range, and held for this process.
///
/// # Errors
///
/// The lock directory cannot be made, or no port in the span is free.
pub fn free_port() -> io::Result<u16> {
    let (low, high) = span();
    let count = u32::from(high - low);
    let start = seed() % count;
    let dir = std::env::temp_dir().join("bulkhead-ports");
    fs::create_dir_all(&dir)?;
    let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
    for i in 0..count {
        let Ok(offset) = u16::try_from((start + i) % count) else {
            continue;
        };
        let port = low + offset;
        let lock = File::create(dir.join(port.to_string()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(e)) => return Err(e),
        }
        if TcpListener::bind(("127.0.0.1", port)).is_err()
            || UdpSocket::bind(("127.0.0.1", port)).is_err()
        {
            continue;
        }
        held.push(lock);
        return Ok(port);
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        format!("no free port in {low}..{high}"),
    ))
}

/// The ports to choose from: below the ephemeral range's start (Linux reads
/// it from /proc; elsewhere 32768, below macOS's 49152).
fn span() -> (u16, u16) {
    let start = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<u16>().ok())
        .unwrap_or(32_768);
    let high = start.min(32_768);
    (high.saturating_sub(SPAN).max(1_024), high)
}

/// Where this process starts looking, so concurrent processes rarely
/// probe the same ports first.
fn seed() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    std::process::id().wrapping_mul(2_654_435_761) ^ nanos
}
