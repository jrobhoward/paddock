//! A rustbridge plugin that misbehaves on request, for S19.
//!
//! | Tag | Payload | Does |
//! | --- | --- | --- |
//! | `echo` | `{"data": "..."}` | returns the payload |
//! | `spin` | `{}` | loops forever |
//! | `abort` | `{}` | aborts the process |
//! | `alloc` | `{"mib": n}` | allocates and touches `n` MiB |
//! | `read` | `{"path": "..."}` | reads a file |
//! | `write` | `{"path": "..."}` | creates a file |
//! | `connect` | `{"addr": "..."}` | opens a TCP connection |
//! | `exec` | `{}` | runs `/bin/true` |

use rustbridge::prelude::*;
use rustbridge::serde_json;

#[derive(Default)]
pub struct S19Plugin;

#[derive(Deserialize)]
struct Alloc {
    mib: usize,
}

#[derive(Deserialize)]
struct Path {
    path: String,
}

#[derive(Deserialize)]
struct Addr {
    addr: String,
}

fn outcome<T, E: std::fmt::Display>(r: Result<T, E>) -> PluginResult<Vec<u8>> {
    match r {
        Ok(_) => Ok(br#"{"ok":true}"#.to_vec()),
        Err(e) => Err(PluginError::HandlerError(e.to_string())),
    }
}

#[async_trait]
impl Plugin for S19Plugin {
    async fn on_start(&self, _ctx: &PluginContext) -> PluginResult<()> {
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
            "spin" => loop {
                std::hint::spin_loop();
            },
            "abort" => std::process::abort(),
            "alloc" => {
                let req: Alloc = serde_json::from_slice(payload)?;
                let v = vec![1u8; req.mib << 20];
                Ok(format!(r#"{{"len":{}}}"#, std::hint::black_box(v).len()).into_bytes())
            }
            "read" => {
                let req: Path = serde_json::from_slice(payload)?;
                outcome(std::fs::read(req.path))
            }
            "write" => {
                let req: Path = serde_json::from_slice(payload)?;
                outcome(std::fs::write(req.path, b"s19"))
            }
            "connect" => {
                let req: Addr = serde_json::from_slice(payload)?;
                let addr = req
                    .addr
                    .parse()
                    .map_err(|e: std::net::AddrParseError| PluginError::HandlerError(e.to_string()))?;
                outcome(std::net::TcpStream::connect_timeout(
                    &addr,
                    std::time::Duration::from_secs(1),
                ))
            }
            "exec" => outcome(std::process::Command::new("/bin/true").status()),
            _ => Err(PluginError::UnknownMessageType(type_tag.to_string())),
        }
    }

    async fn on_stop(&self, _ctx: &PluginContext) -> PluginResult<()> {
        Ok(())
    }

    fn supported_types(&self) -> Vec<&'static str> {
        vec!["echo", "spin", "abort", "alloc", "read", "write", "connect", "exec"]
    }
}

rustbridge_entry!(S19Plugin::default);

pub use rustbridge::ffi_exports::*;
