//! TCP from apps (`wasi:sockets`), with readiness as messages
//! (`bulkhead:io`), as Erlang's `{active, once}`.
//!
//! An instance never blocks waiting for a peer between callbacks: it asks
//! to hear when a stream has bytes ([`TcpStream::notify_readable`]) or a
//! listener has a connection ([`TcpListener::notify_acceptable`]), returns,
//! and handles the host event in `handle_info` ([`ready_tag`]), reading
//! without blocking. A notice comes once per request; ask again after
//! reading. Connecting blocks within the callback, as time in the node (not
//! counted against the callback deadline). The node makes each socket on
//! the manifest's rules, passes a stream to the app and watches readiness
//! itself; reading and writing are the app's own system calls, counted
//! against the callback deadline as its code is.
//!
//! The manifest declares `wasi:sockets` with the addresses the app may
//! connect to, bind and listen on; anything else fails with
//! [`NetError::Denied`].
//!
//! ```no_run
//! use bulkhead_sdk::net::{self, TcpStream};
//! use bulkhead_sdk::{Ctx, Info};
//!
//! # fn demo(ctx: &mut Ctx, info: Info) -> Result<(), net::NetError> {
//! let mut stream = TcpStream::connect("10.0.0.5:1883".parse().unwrap())?;
//! stream.write_all(b"hello")?;
//! stream.notify_readable(1)?;
//! // Later, in handle_info:
//! if net::ready_tag(&info) == Some(1) {
//!     let bytes = stream.read(64 * 1024)?;
//!     stream.notify_readable(1)?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! UDP goes through the node, with `bulkhead_native::app::Ctx::net`.

mod tcp;
pub use tcp::{TcpListener, TcpStream};

use crate::Info;

/// The host event a ready socket sends.
pub const READY_EVENT: &str = "io-ready";

/// Why a socket operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetError {
    /// Not allowed: the capability is not granted, or the address is not
    /// in the manifest's rules.
    Denied,
    /// The app already holds `max_sockets` sockets, or the instance waits
    /// on as many sockets as it may.
    Limited,
    /// The peer closed the stream.
    Closed,
    /// Anything else, as the host reported it.
    Other(String),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied => f.write_str("denied"),
            Self::Limited => f.write_str("limited"),
            Self::Closed => f.write_str("closed"),
            Self::Other(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for NetError {}

/// The tag of a [`READY_EVENT`], if `info` is one.
#[must_use]
pub fn ready_tag(info: &Info) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct Ready {
        tag: u64,
    }
    match info {
        Info::Event { name, msg } if name == READY_EVENT => {
            serde_json::from_slice::<Ready>(&msg.body)
                .ok()
                .map(|r| r.tag)
        }
        _ => None,
    }
}
