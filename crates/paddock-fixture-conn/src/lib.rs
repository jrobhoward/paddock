//! Test fixture: a connection handler (`paddock:connection`) speaking a
//! line protocol, one command per line, one reply line each:
//!
//! - `echo <text>`: reply `<text>` (`v2:<text>` with the `v2` feature).
//! - `info`: reply `<listener> <peer>`.
//! - `self`: reply this handler's pid, `node.id.incarnation`.
//! - `call <name> <body>`: call the instance registered as `<name>` with a
//!   text body; reply its reply, or `err:<error>`.
//! - `quit`: close the connection normally.
//! - `error`: return an error (a crash with a reason).
//! - `crash`: panic (a trap).
//! - `spin`: loop forever without I/O (the idle timeout ends it).

#[allow(missing_docs, clippy::too_many_arguments)]
mod bindings {
    wit_bindgen::generate!({
        path: ["../../wit", "../../wit/vendor/wasi-io", "../../wit/connection"],
        world: "paddock:connection/connection-handler",
        generate_all,
    });
}

use bindings::exports::paddock::connection::handler::{ConnectionInfo, Guest};
use bindings::paddock::runtime::process;
use bindings::paddock::runtime::types::Envelope;
use bindings::wasi::io::streams::{InputStream, OutputStream};

struct Handler;

fn command(info: &ConnectionInfo, line: &str) -> Result<Option<String>, String> {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    Ok(Some(match verb {
        "echo" if cfg!(feature = "v2") => format!("v2:{rest}"),
        "echo" => rest.to_string(),
        "info" => format!("{} {}", info.listener, info.peer),
        "self" => {
            let p = process::self_();
            format!(
                "{}.{}.{}@{}",
                p.node,
                p.id & ((1 << 40) - 1),
                p.incarnation,
                p.id >> 40
            )
        }
        "call" => {
            let (name, body) = rest.split_once(' ').unwrap_or((rest, ""));
            match process::whereis(name) {
                None => "err:noproc".to_string(),
                Some(pid) => {
                    let msg = Envelope {
                        content_type: "text/plain".into(),
                        schema: None,
                        body: body.as_bytes().to_vec(),
                    };
                    match process::call(pid, &msg, 1000) {
                        Ok(reply) => String::from_utf8_lossy(&reply.body).into_owned(),
                        Err(e) => format!("err:{e:?}"),
                    }
                }
            }
        }
        "quit" => return Ok(None),
        "error" => return Err("asked to fail".into()),
        "crash" => panic!("asked to crash"),
        "spin" => {
            let mut n = 0u64;
            loop {
                n = std::hint::black_box(n.wrapping_add(1));
            }
        }
        other => format!("unknown command {other}"),
    }))
}

impl Guest for Handler {
    fn serve(info: ConnectionInfo, input: InputStream, output: OutputStream) -> Result<(), String> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            while let Some(end) = buf.iter().position(|b| *b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..end]).trim_end().to_string();
                buf.drain(..=end);
                match command(&info, &line)? {
                    Some(reply) => {
                        let out = format!("{reply}\n");
                        if output.blocking_write_and_flush(out.as_bytes()).is_err() {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }
            match input.blocking_read(4096) {
                Ok(bytes) => buf.extend_from_slice(&bytes),
                Err(_) => return Ok(()), // the client closed
            }
        }
    }
}

bindings::export!(Handler with_types_in bindings);
