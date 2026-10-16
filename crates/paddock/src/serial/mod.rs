//! Serial lines (ROADMAP F5): a raw tty layer, the frames the failover
//! pair's heartbeat path sends over a serial cable, and a relay that joins
//! two ttys for tests and the soak. Unix only: the common Rust serial
//! crates are MPL-2.0, which the licence policy refuses, so the tty layer
//! is Paddock's own, on `rustix`. The heartbeat path itself is in the node.

pub mod frame;
pub mod relay;
pub mod tty;

pub use relay::relay;
pub use tty::{Tty, pty};
