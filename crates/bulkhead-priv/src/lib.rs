//! `bulkhead-priv`: Bulkhead's privileged helper (ROADMAP PS1, spike S36).
//!
//! Started as root by the service manager, it starts the node under the
//! node's user with a seqpacket socket on descriptor 3 (its number in
//! `BULKHEAD_PRIV_FD`), then does only what the node asks on it
//! ([`bulkhead_priv_wire`]): start an instance, signal one, bind a low
//! port. Every check comes from its own configuration ([`config`]), not
//! from the node: the users apps run as, the runner every instance starts
//! as, where an instance's directories may be, the limits' ceilings and
//! the ports.
//!
//! Each app runs under a user of its own, picked and remembered by the
//! helper ([`bindings`]). On Linux each instance gets a cgroup, new
//! mount, PID, network, IPC and UTS namespaces and a minimal root; on
//! FreeBSD a process group, `RLIMIT_AS` and `rctl` rules (jails are
//! ROADMAP FB2). The instance's directories are shared with the app's
//! group alone ([`share`]).
//!
//! The helper is synchronous and single-threaded, with no async runtime.
//! Its `unsafe` code is in one module of system calls, `sys`, with safe
//! functions over it; the rest of the crate denies `unsafe`. The wire
//! format it parses is a crate of its own that forbids it.

#![deny(unsafe_code)]

pub mod bindings;
pub mod check;
mod child;
pub mod config;
#[cfg(target_os = "freebsd")]
#[path = "freebsd.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
mod serve;
pub mod share;
mod socket;
mod sys;

use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::ExitCode;

pub use serve::Mode;

const USAGE: &str = "usage: bulkhead-priv CONFIG\n       bulkhead-priv --inherited-socket CONFIG   (serve the node on descriptor 3; for tests)";

/// The helper's `main`.
#[must_use]
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (inherited, path) = match args.as_slice() {
        [path] if !path.starts_with('-') => (false, path),
        [flag, path] if flag == "--inherited-socket" => (true, path),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let config = match config::Config::load(Path::new(path)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bulkhead-priv: {e}");
            return ExitCode::from(2);
        }
    };
    let mode = if inherited {
        match inherited_socket() {
            Ok(fd) => Mode::Inherited(fd),
            Err(e) => {
                eprintln!("bulkhead-priv: descriptor 3: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        Mode::StartNode
    };
    match serve::run(config, mode) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("bulkhead-priv: {e}");
            ExitCode::from(1)
        }
    }
}

/// Descriptor 3, checked to be a seqpacket socket and made close-on-exec.
fn inherited_socket() -> std::io::Result<OwnedFd> {
    let fd = sys::adopt(3)?;
    if rustix::net::sockopt::socket_type(&fd)? != rustix::net::SocketType::SEQPACKET {
        return Err(std::io::Error::other("not a seqpacket socket"));
    }
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
    Ok(fd)
}
