//! Write Bulkhead apps in Rust.
//!
//! An app is a type implementing [`App`]: `init` builds its state, and the
//! host then delivers one message at a time to `handle_call`, `handle_cast`
//! and `handle_info`, with a [`Ctx`] for messaging, links, monitors, timers
//! and the registry. [`app!`] generates the program's entry point.
//!
//! ```ignore
//! use bulkhead_sdk::{App, Ctx, CallRef, Envelope, Reply};
//!
//! #[derive(Default)]
//! struct Counter { count: u64 }
//!
//! impl App for Counter {
//!     fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
//!         Ok(Self::default())
//!     }
//!
//!     fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
//!         if msg.as_text() == Some("inc") {
//!             self.count += 1;
//!         }
//!         Reply::Now(Envelope::text(self.count.to_string()))
//!     }
//! }
//!
//! bulkhead_sdk::app!(Counter);
//! ```
//!
//! An app is a program: `app!` generates `bulkhead_main()` for a binary's
//! `main`, and the SDK's calls go over `bulkhead-native`'s protocol
//! (WRITING_APPS.md). Bundle it with `bulkhead bundle`.
//!
//! Host services, each needing its capability in the manifest:
//! - [`log`] and its macros (`info!` and friends): `bulkhead:logging`, always
//!   available.
//! - [`batch`]: many small updates in one message, and reading them back.
//! - [`files`]: the paths of preopened files (`bulkhead:files`).
//! - [`extension`]: calls to extensions, the host services a node is built
//!   with from outside Bulkhead.
//! - `config` (cargo feature `config`): `bulkhead:config`.
//! - `kv` (cargo feature `keyvalue`): `bulkhead:keyvalue` buckets.
//! - `groups` (cargo feature `groups`): `bulkhead:groups`, process groups
//!   on this node with publish.
//! - `http` (cargo feature `http`): requests from the node's HTTP ingress,
//!   delivered as calls, and the responses to them.
//! - `http_client` (cargo feature `http-client`): requests from the app
//!   to hosts its manifest allows (`bulkhead:http-egress`).
//! - `metrics` (cargo feature `metrics`): `bulkhead:metrics`, counters,
//!   gauges and histograms the node exports.
//! - `net` (cargo feature `net`): TCP to and from addresses the manifest
//!   allows (`bulkhead:sockets`), with readiness delivered as host events
//!   (`bulkhead:io`).
//! - `secrets` (cargo feature `secrets`): `bulkhead:secrets`, values the
//!   operator binds on the node.
//! - `watch` (cargo feature `watch`): `bulkhead:watch`, revisioned reads,
//!   compare-and-swap and change watches on keyvalue buckets.
//!
//! Connection handlers (cargo feature `connection`) implement
//! `connection::Handler` instead of [`App`]; an instance holds many
//! connections.
//!
//! A panic ends the instance with `crashed` and its supervisor restarts
//! it. The SDK panics only for states that are bugs (a snapshot that cannot
//! serialize).

extern crate self as bulkhead_sdk;

mod app;
pub mod batch;
#[cfg(feature = "config")]
pub mod config;
#[cfg(feature = "connection")]
pub mod connection;
mod ctx;
mod envelope;
pub mod extension;
pub mod files;
#[cfg(feature = "groups")]
pub mod groups;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "http-client")]
pub mod http_client;
#[cfg(feature = "keyvalue")]
pub mod kv;
pub mod log;
#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(feature = "net")]
pub mod net;
#[cfg(feature = "secrets")]
pub mod secrets;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub mod sender;
pub mod snapshot;
#[cfg(feature = "sql")]
pub mod sql;
mod types;
#[cfg(feature = "watch")]
pub mod watch;

pub use app::{App, Reply};
pub use bulkhead_sdk_macros::Snapshot;
pub use ctx::Ctx;
pub use envelope::{CBOR, Envelope, JSON, MessageError, TEXT};
pub use snapshot::Snapshot;
pub use types::{
    CallError, CallRef, CastError, ExitReason, Info, MonitorRef, Pid, RegisterError, TimerRef,
};

#[doc(hidden)]
pub mod __private {
    pub use crate::app::serve;
}

#[cfg(test)]
#[path = "formats_tests.rs"]
mod formats_tests;
