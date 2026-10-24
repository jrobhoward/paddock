//! A rustbridge plugin for tests, run as a native-process app through
//! `paddock-runner --library`.
//!
//! | Type tag | Request | Response |
//! | --- | --- | --- |
//! | `echo` | any JSON | the same |
//! | `read` | `{"path": "..."}` | `{"ok": true}`, or an error |
//! | `fail` | any | an error |
//! | `config` | any | the plugin config's `data` |

use rustbridge::prelude::*;
use rustbridge::{serde_json, tracing};

/// The plugin.
#[derive(Debug, Default)]
pub struct FixturePlugin {
    data: std::sync::OnceLock<serde_json::Value>,
}

#[derive(Deserialize)]
struct Read {
    path: String,
}

#[async_trait]
impl Plugin for FixturePlugin {
    async fn on_start(&self, ctx: &PluginContext) -> PluginResult<()> {
        let _ = self.data.set(ctx.config.data.clone());
        tracing::info!("fixture plugin started");
        Ok(())
    }

    async fn handle_request(
        &self,
        _ctx: &PluginContext,
        type_tag: &str,
        payload: &[u8],
    ) -> PluginResult<Vec<u8>> {
        match type_tag {
            "echo" => Ok(payload.to_vec()),
            "read" => {
                let req: Read = serde_json::from_slice(payload)?;
                std::fs::read(&req.path)
                    .map(|_| br#"{"ok":true}"#.to_vec())
                    .map_err(|e| PluginError::HandlerError(e.to_string()))
            }
            "config" => Ok(serde_json::to_vec(
                self.data.get().unwrap_or(&serde_json::Value::Null),
            )?),
            "fail" => Err(PluginError::HandlerError("asked to fail".into())),
            _ => Err(PluginError::UnknownMessageType(type_tag.to_string())),
        }
    }

    async fn on_stop(&self, _ctx: &PluginContext) -> PluginResult<()> {
        Ok(())
    }

    fn supported_types(&self) -> Vec<&'static str> {
        vec!["echo", "read", "config", "fail"]
    }
}

rustbridge_entry!(FixturePlugin::default);

pub use rustbridge::ffi_exports::*;
