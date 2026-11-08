//! The `App` trait, and serving one over `bulkhead-native`'s protocol
//! behind `app!`.

use std::process::ExitCode;

use bulkhead_native::app as n;
use bulkhead_native::protocol as p;

use crate::ctx::Ctx;
use crate::envelope::Envelope;
use crate::types::{CallRef, ExitReason, Info};

/// The schema of the host's health check calls and their replies; the SDK
/// answers them from [`App::health`], so `handle_call` never sees them.
const HEALTH_SCHEMA: &str = "bulkhead/health";

/// What `handle_call` does with the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Answer now.
    Now(Envelope),
    /// Answer later with [`Ctx::reply`].
    Later,
}

/// A Bulkhead app: a `gen_server`-style set of callbacks over its state.
///
/// The host calls one callback at a time. Only `init` and `handle_call` are
/// required; the rest default to doing nothing, and the snapshot pair
/// defaults to refusing snapshot upgrades (`restart` mode still works).
pub trait App: Sized + 'static {
    /// Build the state. `args` is the manifest's `lifecycle.start_args` as
    /// JSON, or empty.
    ///
    /// # Errors
    ///
    /// A message to fail the start; the supervisor counts it as a restart.
    fn init(ctx: &mut Ctx, args: &[u8]) -> Result<Self, String>;

    /// Handle a call.
    fn handle_call(&mut self, ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Reply;

    /// Handle a cast.
    fn handle_cast(&mut self, ctx: &mut Ctx, msg: Envelope) {
        let _ = (ctx, msg);
    }

    /// Handle a link exit, monitor notification, timer or host event.
    fn handle_info(&mut self, ctx: &mut Ctx, info: Info) {
        let _ = (ctx, info);
    }

    /// Clean up before a graceful stop. Not called on kills or traps.
    fn terminate(&mut self, ctx: &mut Ctx, reason: ExitReason) {
        let _ = (ctx, reason);
    }

    /// Whether this instance is healthy, for the host's health checks: an
    /// app whose manifest has a `health` section is asked every
    /// `interval_ms`, and after `failure_threshold` failures in a row the
    /// instance is killed as `crashed: unhealthy` and restarted. Healthy by
    /// default; the check also fails if the instance does not answer in
    /// time.
    ///
    /// # Errors
    ///
    /// Why it is not healthy; the host logs it.
    fn health(&mut self, ctx: &mut Ctx) -> Result<(), String> {
        let _ = ctx;
        Ok(())
    }

    /// Serialize the state for an upgrade. Must not change it: if the
    /// upgrade fails, this instance keeps running.
    ///
    /// # Errors
    ///
    /// A message if the state cannot be serialized; the instance then traps
    /// and its supervisor restarts the old version.
    fn save_snapshot(&self) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }

    /// Build the state from a snapshot written by version `from_version` of
    /// this app.
    ///
    /// # Errors
    ///
    /// A message to refuse it; the host keeps the old version running.
    fn load_snapshot(ctx: &mut Ctx, from_version: &str, snapshot: &[u8]) -> Result<Self, String> {
        let _ = (ctx, snapshot);
        Err(format!(
            "this version does not load snapshots (from {from_version}); upgrade in restart mode"
        ))
    }
}

/// Serve `$ty` (an [`App`]) as a program: generates `bulkhead_main()`, for
/// `main` in a binary of the app's crate to call. Use once per app crate.
#[macro_export]
macro_rules! app {
    ($ty:ty) => {
        /// Serve this app until the node stops it: call from `main` in a
        /// binary of the app's crate.
        pub fn bulkhead_main() -> ::std::process::ExitCode {
            $crate::__private::serve::<$ty>()
        }
    };
}

/// An SDK app as a `bulkhead-native` one.
struct Adapter<A>(A);

impl<A: App> n::App for Adapter<A> {
    fn init(_ctx: &mut n::Ctx, args: Vec<u8>) -> Result<Self, String> {
        A::init(&mut Ctx::new(), &args).map(Adapter)
    }

    fn handle_call(
        &mut self,
        _ctx: &mut n::Ctx,
        call: n::CallRef,
        msg: p::Envelope,
    ) -> Option<p::Envelope> {
        // The host's health check goes to `App::health`.
        if msg.schema.as_deref() == Some(HEALTH_SCHEMA) {
            let body = match self.0.health(&mut Ctx::new()) {
                Ok(()) => "ok".to_string(),
                Err(why) => why,
            };
            return Some(
                Envelope::new("text/plain", body)
                    .with_schema(HEALTH_SCHEMA)
                    .into(),
            );
        }
        match self
            .0
            .handle_call(&mut Ctx::new(), CallRef(call), msg.into())
        {
            Reply::Now(env) => Some(env.into()),
            Reply::Later => None,
        }
    }

    fn handle_cast(&mut self, _ctx: &mut n::Ctx, msg: p::Envelope) {
        self.0.handle_cast(&mut Ctx::new(), msg.into());
    }

    fn handle_info(&mut self, _ctx: &mut n::Ctx, info: p::Info) {
        // The SDK's own event: the host's effective log level.
        if let p::Info::HostEvent { name, msg } = &info
            && name == crate::log::LEVEL_EVENT
        {
            crate::log::set_from_host(&msg.body);
            return;
        }
        self.0.handle_info(&mut Ctx::new(), info.into());
    }

    fn terminate(&mut self, _ctx: &mut n::Ctx, reason: p::ExitReason) {
        self.0.terminate(&mut Ctx::new(), reason.into());
    }

    fn save_snapshot(&self) -> Result<Vec<u8>, String> {
        self.0.save_snapshot()
    }

    fn load_snapshot(
        _ctx: &mut n::Ctx,
        from_version: &str,
        snapshot: Vec<u8>,
    ) -> Result<Self, String> {
        A::load_snapshot(&mut Ctx::new(), from_version, &snapshot).map(Adapter)
    }
}

/// Serve `A` until the node stops it.
#[must_use]
pub fn serve<A: App>() -> ExitCode {
    n::serve::<Adapter<A>>()
}
