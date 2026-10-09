//! Write Paddock apps in Rust.
//!
//! An app is a type implementing [`App`]: `init` builds its state, and the
//! host then delivers one message at a time to `handle_call`, `handle_cast`
//! and `handle_info`, with a [`Ctx`] for messaging, links, monitors, timers
//! and the registry. [`app!`] exports the type as a component.
//!
//! ```ignore
//! use paddock_sdk::{App, Ctx, CallRef, Envelope, Reply};
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
//! paddock_sdk::app!(Counter);
//! ```
//!
//! Build for `wasm32-wasip2` and bundle with `paddock bundle`.
//!
//! Host services, each needing its capability in the manifest:
//! - [`log`] and its macros (`info!` and friends): `wasi:logging`, always
//!   available.
//! - `config` (cargo feature `config`): `wasi:config`.
//! - `kv` (cargo feature `keyvalue`): `wasi:keyvalue` buckets.
//! - `groups` (cargo feature `groups`): `paddock:groups`, process groups
//!   on this node with publish.
//! - `watch` (cargo feature `watch`): `paddock:watch`, revisioned reads,
//!   compare-and-swap and change watches on keyvalue buckets.
//!
//! Connection handlers (cargo feature `connection`) implement `Connection`
//! instead of [`App`]: one instance per connection on the app's listeners.
//!
//! In a guest, a panic is a trap: the host ends the instance with
//! `crashed` and its supervisor restarts it. The SDK panics only for
//! states that are bugs (a callback before a successful `init`, a snapshot
//! that cannot serialize).

extern crate self as paddock_sdk;

mod app;
#[cfg(feature = "config")]
pub mod config;
#[cfg(feature = "connection")]
pub mod connection;
mod ctx;
mod envelope;
#[cfg(feature = "groups")]
pub mod groups;
#[cfg(feature = "keyvalue")]
pub mod kv;
pub mod log;
pub mod snapshot;
mod types;
#[cfg(feature = "watch")]
pub mod watch;

#[doc(hidden)]
#[allow(missing_docs, clippy::too_many_arguments)]
pub mod bindings {
    wit_bindgen::generate!({
        path: [
            "../../wit",
            "../../wit/vendor/wasi-logging",
            "../../wit/vendor/wasi-config",
            "../../wit/vendor/wasi-keyvalue",
            "../../wit/vendor/wasi-io",
            "../../wit/connection",
            "../../wit/groups",
            "../../wit/watch",
            "wit",
        ],
        world: "paddock:sdk/app",
        pub_export_macro: true,
        default_bindings_module: "paddock_sdk::bindings",
        generate_all,
    });
}

pub use app::{App, Reply};
pub use ctx::Ctx;
pub use envelope::{CBOR, Envelope, JSON, MessageError, TEXT};
pub use paddock_sdk_macros::Snapshot;
pub use snapshot::Snapshot;
pub use types::{CallError, CallRef, ExitReason, Info, MonitorRef, Pid, RegisterError, TimerRef};

#[doc(hidden)]
pub mod __private {
    pub use crate::app::dispatch::*;
}
