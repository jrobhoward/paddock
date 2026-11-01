//! The `App` trait and the export glue behind `app!`.

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

/// Export `$ty` (an [`App`]) as the component's `bulkhead:runtime/app`
/// implementation. Use once per app crate.
#[macro_export]
macro_rules! app {
    ($ty:ty) => {
        #[doc(hidden)]
        struct __BulkheadApp;

        ::std::thread_local! {
            static __BULKHEAD_STATE: ::core::cell::RefCell<::core::option::Option<$ty>> =
                const { ::core::cell::RefCell::new(::core::option::Option::None) };
        }

        impl $crate::bindings::Guest for __BulkheadApp {
            fn init(args: ::std::vec::Vec<u8>) -> ::core::result::Result<(), ::std::string::String> {
                __BULKHEAD_STATE.with(|s| $crate::__private::init::<$ty>(s, &args))
            }
            fn handle_call(
                caller: $crate::bindings::CallRef,
                msg: $crate::bindings::Envelope,
            ) -> $crate::bindings::CallResult {
                __BULKHEAD_STATE.with(|s| $crate::__private::handle_call::<$ty>(s, caller, msg))
            }
            fn handle_cast(msg: $crate::bindings::Envelope) {
                __BULKHEAD_STATE.with(|s| $crate::__private::handle_cast::<$ty>(s, msg))
            }
            fn handle_info(msg: $crate::bindings::Info) {
                __BULKHEAD_STATE.with(|s| $crate::__private::handle_info::<$ty>(s, msg))
            }
            fn terminate(reason: $crate::bindings::ExitReason) {
                __BULKHEAD_STATE.with(|s| $crate::__private::terminate::<$ty>(s, reason))
            }
            fn save_snapshot() -> ::std::vec::Vec<u8> {
                __BULKHEAD_STATE.with(|s| $crate::__private::save_snapshot::<$ty>(s))
            }
            fn load_snapshot(
                from_version: ::std::string::String,
                snapshot: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<(), ::std::string::String> {
                __BULKHEAD_STATE.with(|s| $crate::__private::load_snapshot::<$ty>(s, &from_version, &snapshot))
            }
        }

        $crate::bindings::export!(__BulkheadApp with_types_in $crate::bindings);

        /// Serve this app as a native program: call from `main` in a
        /// binary of the app's crate, run with `native.program =
        /// "executable"`.
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        pub fn bulkhead_main() -> ::std::process::ExitCode {
            $crate::__private::serve::<$ty>()
        }
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
        panic!("bulkhead-sdk: callback before a successful init")
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
            Err(why) => panic!("bulkhead-sdk: save_snapshot failed: {why}"),
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

/// The same callbacks served as a native program, over
/// `bulkhead-native`'s protocol.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub mod native {
    use std::process::ExitCode;

    use bulkhead_native::app as n;
    use bulkhead_native::protocol as p;

    use super::{App, HEALTH_SCHEMA, Reply};
    use crate::bindings::bulkhead::runtime::types as wit;
    use crate::ctx::Ctx;
    use crate::envelope::Envelope;
    use crate::host::native::{wit_envelope, wit_exit_reason, wit_pid};
    use crate::types::{CallRef, Info};

    fn sdk_envelope(env: p::Envelope) -> Envelope {
        wit_envelope(env).into()
    }

    fn native_envelope(env: Envelope) -> p::Envelope {
        crate::host::native::envelope(&env.into())
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
                return Some(native_envelope(
                    Envelope::new("text/plain", body).with_schema(HEALTH_SCHEMA),
                ));
            }
            match self
                .0
                .handle_call(&mut Ctx::new(), CallRef(call), sdk_envelope(msg))
            {
                Reply::Now(env) => Some(native_envelope(env)),
                Reply::Later => None,
            }
        }

        fn handle_cast(&mut self, _ctx: &mut n::Ctx, msg: p::Envelope) {
            self.0.handle_cast(&mut Ctx::new(), sdk_envelope(msg));
        }

        fn handle_info(&mut self, _ctx: &mut n::Ctx, info: p::Info) {
            let info = match info {
                // The SDK's own event: the host's effective log level.
                p::Info::HostEvent { name, msg } if name == crate::log::LEVEL_EVENT => {
                    crate::log::set_from_host(&msg.body);
                    return;
                }
                p::Info::HostEvent { name, msg } => wit::Info::HostEvent((name, wit_envelope(msg))),
                p::Info::Exit { from, reason } => {
                    wit::Info::Exit((wit_pid(from), wit_exit_reason(reason)))
                }
                p::Info::Down {
                    reference,
                    pid,
                    reason,
                } => wit::Info::Down((reference, wit_pid(pid), wit_exit_reason(reason))),
                p::Info::Timer { reference, msg } => {
                    wit::Info::Timer((reference, wit_envelope(msg)))
                }
                p::Info::KvChanged {
                    watch,
                    bucket,
                    key,
                    entry,
                    revision,
                } => {
                    // As a component's notice, plus what the node read
                    // when it delivered it (`watch::Change::carried`).
                    let (value, revision) = match entry {
                        Some((value, at)) => (Some(value.0), at),
                        None => (None, revision),
                    };
                    let body = serde_json::json!({
                        "watch": watch,
                        "bucket": bucket,
                        "key": key,
                        "revision": revision,
                        "carried": true,
                        "value": value,
                    });
                    wit::Info::HostEvent((
                        "kv-changed".to_string(),
                        wit_envelope(p::Envelope::new("application/json", body.to_string())),
                    ))
                }
            };
            self.0.handle_info(&mut Ctx::new(), Info::from(info));
        }

        fn terminate(&mut self, _ctx: &mut n::Ctx, reason: p::ExitReason) {
            self.0
                .terminate(&mut Ctx::new(), wit_exit_reason(reason).into());
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

    /// Serve `A` as a native program until the node stops it.
    #[must_use]
    pub fn serve<A: App>() -> ExitCode {
        n::serve::<Adapter<A>>()
    }
}
