//! Paddock: a clustered application server for sandboxed WebAssembly
//! components, with Erlang/OTP-style mailboxes, links, supervision and hot
//! upgrade.
//!
//! This crate is the `paddock` command and the library behind it:
//!
//! - [`api`]: the admin API's request and response types, and a client over
//!   the node's Unix socket.
//! - `node` (the default `node` feature): the node daemon, which
//!   `paddock node` runs. Without the feature the crate has the CLI and
//!   bundle tooling only, and does not build the runtime.
//!
//! The design is in `docs/DESIGN.md` and the plan in `docs/ROADMAP.md`.

pub mod api;
#[cfg(feature = "node")]
pub mod node;
#[cfg(unix)]
pub mod serial;
