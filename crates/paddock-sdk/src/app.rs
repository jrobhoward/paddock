//! The `App` trait and the export glue behind `app!`.

use crate::ctx::Ctx;
use crate::envelope::Envelope;
use crate::types::{CallRef, ExitReason, Info};

/// The schema of the host's health check calls and their replies; the SDK
/// answers them from [`App::health`], so `handle_call` never sees them.
const HEALTH_SCHEMA: &str = "paddock/health";

/// What `handle_call` does with the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Answer now.
    Now(Envelope),
    /// Answer later with [`Ctx::reply`].
    Later,
}

/// A Paddock app: a `gen_server`-style set of callbacks over its state.
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

/// Export `$ty` (an [`App`]) as the component's `paddock:runtime/app`
/// implementation. Use once per app crate.
#[macro_export]
macro_rules! app {
    ($ty:ty) => {
        #[doc(hidden)]
        struct __PaddockApp;

        ::std::thread_local! {
            static __PADDOCK_STATE: ::core::cell::RefCell<::core::option::Option<$ty>> =
                const { ::core::cell::RefCell::new(::core::option::Option::None) };
        }

        impl $crate::bindings::Guest for __PaddockApp {
            fn init(args: ::std::vec::Vec<u8>) -> ::core::result::Result<(), ::std::string::String> {
                __PADDOCK_STATE.with(|s| $crate::__private::init::<$ty>(s, &args))
            }
            fn handle_call(
                caller: $crate::bindings::CallRef,
                msg: $crate::bindings::Envelope,
            ) -> $crate::bindings::CallResult {
                __PADDOCK_STATE.with(|s| $crate::__private::handle_call::<$ty>(s, caller, msg))
            }
            fn handle_cast(msg: $crate::bindings::Envelope) {
                __PADDOCK_STATE.with(|s| $crate::__private::handle_cast::<$ty>(s, msg))
            }
            fn handle_info(msg: $crate::bindings::Info) {
                __PADDOCK_STATE.with(|s| $crate::__private::handle_info::<$ty>(s, msg))
            }
            fn terminate(reason: $crate::bindings::ExitReason) {
                __PADDOCK_STATE.with(|s| $crate::__private::terminate::<$ty>(s, reason))
            }
            fn save_snapshot() -> ::std::vec::Vec<u8> {
                __PADDOCK_STATE.with(|s| $crate::__private::save_snapshot::<$ty>(s))
            }
            fn load_snapshot(
                from_version: ::std::string::String,
                snapshot: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<(), ::std::string::String> {
                __PADDOCK_STATE.with(|s| $crate::__private::load_snapshot::<$ty>(s, &from_version, &snapshot))
            }
        }

        $crate::bindings::export!(__PaddockApp with_types_in $crate::bindings);
    };
}

/// The generic halves of the callbacks `app!` exports.
pub mod dispatch {
    use std::cell::RefCell;

    use super::{App, HEALTH_SCHEMA, Reply};
    use crate::bindings;
    use crate::ctx::Ctx;
    use crate::envelope::Envelope;

    /// A callback arrived with no state: `init` did not succeed. The host
    /// never does this, so it is a bug; trap.
    fn no_state() -> ! {
        panic!("paddock-sdk: callback before a successful init")
    }

    fn with<A: App, R>(cell: &RefCell<Option<A>>, f: impl FnOnce(&mut A, &mut Ctx) -> R) -> R {
        let mut state = cell.borrow_mut();
        let Some(app) = state.as_mut() else {
            no_state()
        };
        f(app, &mut Ctx::new())
    }

    /// `init`.
    ///
    /// # Errors
    ///
    /// The app's `init` error.
    pub fn init<A: App>(cell: &RefCell<Option<A>>, args: &[u8]) -> Result<(), String> {
        let app = A::init(&mut Ctx::new(), args)?;
        *cell.borrow_mut() = Some(app);
        Ok(())
    }

    /// `handle-call`.
    pub fn handle_call<A: App>(
        cell: &RefCell<Option<A>>,
        caller: bindings::CallRef,
        msg: bindings::Envelope,
    ) -> bindings::CallResult {
        // The host's health check goes to `App::health`.
        if msg.schema.as_deref() == Some(HEALTH_SCHEMA) {
            let answer = with(cell, |app, ctx| app.health(ctx));
            let body = match answer {
                Ok(()) => "ok".to_string(),
                Err(why) => why,
            };
            return bindings::CallResult::Reply(
                Envelope::new("text/plain", body)
                    .with_schema(HEALTH_SCHEMA)
                    .into(),
            );
        }
        match with(cell, |app, ctx| {
            app.handle_call(ctx, crate::CallRef(caller), msg.into())
        }) {
            Reply::Now(env) => bindings::CallResult::Reply(env.into()),
            Reply::Later => bindings::CallResult::Noreply,
        }
    }

    /// `handle-cast`.
    pub fn handle_cast<A: App>(cell: &RefCell<Option<A>>, msg: bindings::Envelope) {
        with(cell, |app, ctx| app.handle_cast(ctx, msg.into()));
    }

    /// `handle-info`.
    pub fn handle_info<A: App>(cell: &RefCell<Option<A>>, info: bindings::Info) {
        // The SDK's own event: the host's effective log level.
        if let bindings::Info::HostEvent((name, msg)) = &info
            && name == crate::log::LEVEL_EVENT
        {
            crate::log::set_from_host(&msg.body);
            return;
        }
        with(cell, |app, ctx| app.handle_info(ctx, info.into()));
    }

    /// `terminate`.
    pub fn terminate<A: App>(cell: &RefCell<Option<A>>, reason: bindings::ExitReason) {
        with(cell, |app, ctx| app.terminate(ctx, reason.into()));
    }

    /// `save-snapshot`. A state that cannot serialize traps: the upgrade
    /// then fails safely and the supervisor restarts the old version.
    pub fn save_snapshot<A: App>(cell: &RefCell<Option<A>>) -> Vec<u8> {
        let state = cell.borrow();
        let Some(app) = state.as_ref() else {
            no_state()
        };
        match app.save_snapshot() {
            Ok(bytes) => bytes,
            Err(why) => panic!("paddock-sdk: save_snapshot failed: {why}"),
        }
    }

    /// `load-snapshot`.
    ///
    /// # Errors
    ///
    /// The app's `load_snapshot` error; the host then keeps the old version.
    pub fn load_snapshot<A: App>(
        cell: &RefCell<Option<A>>,
        from_version: &str,
        snapshot: &[u8],
    ) -> Result<(), String> {
        let app = A::load_snapshot(&mut Ctx::new(), from_version, snapshot)?;
        *cell.borrow_mut() = Some(app);
        Ok(())
    }
}
