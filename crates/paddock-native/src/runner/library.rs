//! A rustbridge plugin as a native app: callbacks and logging only, since
//! rustbridge's FFI has no call into the host.
//!
//! | Callback | Plugin |
//! | --- | --- |
//! | `init` | loads the plugin; the init arguments (JSON) become its config's `data` |
//! | call | `handle_request(type_tag, payload)` with the envelope's schema id as the type tag; the reply carries the same schema id |
//! | cast | the same, with the response dropped |
//! | info | ignored |
//! | `terminate` | stops the plugin |
//!
//! Requests and responses are JSON (`application/json`); a request in
//! another content type, or a plugin error, is answered with schema id
//! [`ERROR_SCHEMA`] and a body `{"error": "..."}`. Plugin logs become log
//! records.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

use rustbridge_consumer::{
    LogCallbackFn, LogLevel, NativePlugin, NativePluginLoader, PluginConfig,
};

use crate::app::{self, App, CallRef, Ctx};
use crate::protocol::{Envelope, ExitReason, Level};

/// The content type a rustbridge plugin takes and returns.
pub const JSON: &str = "application/json";

/// The schema id of an error reply.
pub const ERROR_SCHEMA: &str = "rustbridge.error";

/// The plugin to load at `init`.
static PATH: OnceLock<PathBuf> = OnceLock::new();

struct Library {
    plugin: NativePlugin,
}

fn level(l: LogLevel) -> Level {
    match l {
        LogLevel::Trace => Level::Trace,
        LogLevel::Debug => Level::Debug,
        LogLevel::Info | LogLevel::Off => Level::Info,
        LogLevel::Warn => Level::Warn,
        LogLevel::Error => Level::Error,
    }
}

/// An error reply.
#[must_use]
pub fn error(why: &str) -> Envelope {
    Envelope {
        content_type: JSON.to_string(),
        schema: Some(ERROR_SCHEMA.to_string()),
        body: serde_json::json!({ "error": why }).to_string().into_bytes(),
    }
}

impl Library {
    fn request(&self, msg: &Envelope) -> Envelope {
        if msg.content_type != JSON {
            return error(&format!(
                "a rustbridge plugin takes {JSON}, not {}",
                msg.content_type
            ));
        }
        let Ok(body) = std::str::from_utf8(&msg.body) else {
            return error("the request is not UTF-8");
        };
        let tag = msg.schema.clone().unwrap_or_default();
        match self.plugin.call(&tag, body) {
            Ok(response) => Envelope {
                content_type: JSON.to_string(),
                schema: Some(tag),
                body: response.into_bytes(),
            },
            Err(e) => error(&e.to_string()),
        }
    }
}

impl App for Library {
    fn init(_ctx: &mut Ctx, args: Vec<u8>) -> Result<Self, String> {
        let path = PATH.get().ok_or("no plugin path")?;
        let mut config = PluginConfig::default();
        if !args.is_empty() {
            config.data =
                serde_json::from_slice(&args).map_err(|e| format!("init arguments: {e}"))?;
        }
        let log: LogCallbackFn = Arc::new(|l, target, message| {
            app::log(level(l), format!("{target}: {message}"));
        });
        let plugin = NativePluginLoader::load_with_config(path, &config, Some(log))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { plugin })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Option<Envelope> {
        Some(self.request(&msg))
    }

    fn handle_cast(&mut self, _ctx: &mut Ctx, msg: Envelope) {
        let response = self.request(&msg);
        if response.schema.as_deref() == Some(ERROR_SCHEMA) {
            app::log(
                Level::Warn,
                format!("cast: {}", String::from_utf8_lossy(&response.body)),
            );
        }
    }

    fn terminate(&mut self, _ctx: &mut Ctx, _reason: ExitReason) {
        if let Err(e) = self.plugin.shutdown() {
            app::log(Level::Warn, format!("plugin shutdown: {e}"));
        }
    }
}

/// Serve the plugin at `path`.
#[must_use]
pub fn serve(path: PathBuf) -> ExitCode {
    let _ = PATH.set(path);
    app::serve::<Library>()
}
