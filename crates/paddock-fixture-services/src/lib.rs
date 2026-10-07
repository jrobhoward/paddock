//! Test fixture: an app that drives Paddock's host services.
//!
//! Message bodies are UTF-8 commands; fields are separated by `:`.
//! - `log:<level>:<message>`: `wasi:logging` with context `fixture`.
//! - `print:<text>` / `eprint:<text>`: a line on stdout / stderr.
//! - `config:<key>`: the value, `none`, or `err:<error>`.
//! - `config-all`: `key=value` pairs joined by `;`, sorted, or `err:<error>`.
//! - `kv-set:<bucket>:<key>:<value>`, `kv-get:<bucket>:<key>`,
//!   `kv-del:<bucket>:<key>`, `kv-exists:<bucket>:<key>`,
//!   `kv-keys:<bucket>` (sorted, comma-separated, every page),
//!   `kv-incr:<bucket>:<key>:<delta>`,
//!   `kv-set-many:<bucket>:<k>=<v>,...`, `kv-get-many:<bucket>:<k>,...`,
//!   `kv-del-many:<bucket>:<k>,...`. Errors reply `err:<error>`.
//! - `kv-fill:<bucket>:<count>`: set `k0`..`k<count-1>` to one byte each.
//! - `read:<path>`, `write:<path>:<text>`: `std::fs`; reply contents / `ok`
//!   or `err:<io error kind>`.
//! - `denied-ping`, `denied-boom`: call the unimplemented `denied` interface
//!   (with the `denied` feature only).
//! - `trap`: panic.
//!
//! Built for `wasm32-wasip2`.

#[allow(missing_docs, clippy::too_many_arguments)]
mod bindings {
    wit_bindgen::generate!({
        path: [
            "../../wit",
            "../../wit/vendor/wasi-logging",
            "../../wit/vendor/wasi-config",
            "../../wit/vendor/wasi-keyvalue",
            "wit",
        ],
        world: "paddock:fixture/services",
        generate_all,
        pub_export_macro: true,
    });
}

#[cfg(feature = "denied")]
use bindings::paddock::fixture::denied;
use bindings::wasi::config::store as config;
use bindings::wasi::keyvalue::{atomics, batch, store};
use bindings::wasi::logging::logging::{self, Level};
use bindings::{CallRef, CallResult, Envelope, ExitReason, Guest, Info};

/// The services app.
struct Services;

fn kv_err(e: store::Error) -> String {
    match e {
        store::Error::NoSuchStore => "err:no-such-store".into(),
        store::Error::AccessDenied => "err:access-denied".into(),
        store::Error::Other(why) => format!("err:other:{why}"),
    }
}

fn config_err(e: config::Error) -> String {
    match e {
        config::Error::Upstream(why) => format!("err:upstream:{why}"),
        config::Error::Io(why) => format!("err:io:{why}"),
    }
}

fn level(name: &str) -> Level {
    match name {
        "trace" => Level::Trace,
        "debug" => Level::Debug,
        "warn" => Level::Warn,
        "error" => Level::Error,
        "critical" => Level::Critical,
        _ => Level::Info,
    }
}

fn io_err(e: std::io::Error) -> String {
    format!("err:{:?}", e.kind())
}

/// Run a keyvalue command against an opened bucket.
fn kv(verb: &str, bucket: &str, rest: &str) -> String {
    let b = match store::open(bucket) {
        Ok(b) => b,
        Err(e) => return kv_err(e),
    };
    let result = match verb {
        "kv-set" => {
            let (k, v) = rest.split_once(':').unwrap_or((rest, ""));
            b.set(k, v.as_bytes()).map(|()| "ok".to_string())
        }
        "kv-get" => b
            .get(rest)
            .map(|v| v.map_or("none".into(), |v| String::from_utf8_lossy(&v).into_owned())),
        "kv-del" => b.delete(rest).map(|()| "ok".to_string()),
        "kv-exists" => b.exists(rest).map(|e| e.to_string()),
        "kv-keys" => {
            let mut keys = Vec::new();
            let mut cursor = None;
            loop {
                match b.list_keys(cursor) {
                    Ok(page) => {
                        keys.extend(page.keys);
                        match page.cursor {
                            Some(c) => cursor = Some(c),
                            None => break,
                        }
                    }
                    Err(e) => return kv_err(e),
                }
            }
            keys.sort();
            Ok(keys.join(","))
        }
        "kv-incr" => {
            let (k, d) = rest.split_once(':').unwrap_or((rest, "1"));
            atomics::increment(&b, k, d.parse().unwrap_or(1)).map(|v| v.to_string())
        }
        "kv-set-many" => {
            let pairs: Vec<(String, Vec<u8>)> = rest
                .split(',')
                .filter_map(|p| p.split_once('='))
                .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                .collect();
            batch::set_many(&b, &pairs).map(|()| "ok".to_string())
        }
        "kv-get-many" => {
            let keys: Vec<String> = rest.split(',').map(String::from).collect();
            batch::get_many(&b, &keys).map(|found| {
                found
                    .into_iter()
                    .map(|f| match f {
                        Some((k, v)) => format!("{k}={}", String::from_utf8_lossy(&v)),
                        None => "-".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
        }
        "kv-del-many" => {
            let keys: Vec<String> = rest.split(',').map(String::from).collect();
            batch::delete_many(&b, &keys).map(|()| "ok".to_string())
        }
        "kv-fill" => {
            let n: usize = rest.parse().unwrap_or(0);
            (0..n)
                .try_for_each(|i| b.set(&format!("k{i}"), b"x"))
                .map(|()| "ok".to_string())
        }
        _ => Ok(format!("unknown {verb}")),
    };
    result.unwrap_or_else(kv_err)
}

fn run(command: &str) -> String {
    let (verb, arg) = command.split_once(':').unwrap_or((command, ""));
    match verb {
        "log" => {
            let (lvl, msg) = arg.split_once(':').unwrap_or(("info", arg));
            logging::log(level(lvl), "fixture", msg);
            "ok".into()
        }
        "print" => {
            println!("{arg}");
            "ok".into()
        }
        "eprint" => {
            eprintln!("{arg}");
            "ok".into()
        }
        "config" => match config::get(arg) {
            Ok(v) => v.unwrap_or_else(|| "none".into()),
            Err(e) => config_err(e),
        },
        "config-all" => match config::get_all() {
            Ok(mut all) => {
                all.sort();
                all.into_iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(";")
            }
            Err(e) => config_err(e),
        },
        v if v.starts_with("kv-") => {
            let (bucket, rest) = arg.split_once(':').unwrap_or((arg, ""));
            kv(v, bucket, rest)
        }
        "read" => std::fs::read_to_string(arg).unwrap_or_else(io_err),
        "write" => {
            let (path, text) = arg.split_once(':').unwrap_or((arg, ""));
            std::fs::write(path, text).map_or_else(io_err, |()| "ok".into())
        }
        #[cfg(feature = "denied")]
        "denied-ping" => match denied::ping() {
            Ok(s) => s,
            Err(denied::Error::AccessDenied) => "err:access-denied".into(),
            Err(denied::Error::Other(why)) => format!("err:other:{why}"),
        },
        #[cfg(feature = "denied")]
        "denied-boom" => {
            denied::boom();
            "returned".into()
        }
        "trap" => panic!("asked to trap"),
        _ => format!("unknown command '{command}'"),
    }
}

impl Guest for Services {
    fn init(_args: Vec<u8>) -> Result<(), String> {
        Ok(())
    }

    fn handle_call(_caller: CallRef, msg: Envelope) -> CallResult {
        let reply = run(&String::from_utf8_lossy(&msg.body));
        CallResult::Reply(Envelope {
            content_type: "text/plain".into(),
            schema: None,
            body: reply.into_bytes(),
        })
    }

    fn handle_cast(msg: Envelope) {
        run(&String::from_utf8_lossy(&msg.body));
    }

    fn handle_info(_msg: Info) {}
    fn terminate(_reason: ExitReason) {}

    fn save_snapshot() -> Vec<u8> {
        Vec::new()
    }

    fn load_snapshot(_from: String, _snapshot: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
}

bindings::export!(Services with_types_in bindings);
