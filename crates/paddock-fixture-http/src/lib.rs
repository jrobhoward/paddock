//! Test fixture: an HTTP handler, written as any `wasi:http/proxy`
//! component is (`wasip2::http::proxy::export!`), plus one Paddock import
//! (`paddock:runtime/process`) to call apps. Paths:
//!
//! - `GET /hello`: `200 hello` (`hello v2` with the `v2` feature).
//! - `GET /self`: this instance's pid, `node.id.incarnation`; a new one
//!   for each request.
//! - `POST /echo`: the request body streamed back as it is read.
//! - `GET /drip?chunks=N&ms=M`: N chunks of `drip\n`, M ms apart.
//! - `GET /call?name=X&body=Y`: call the instance registered as X with
//!   text Y; its reply, or `502 err:<error>`.
//! - `GET /trap`: panic before answering (a trap).
//! - `GET /trap-late`: send headers and one chunk, then panic.
//! - `GET /spin`: loop forever (the callback deadline ends it).
//! - `GET /hang`: sleep 60 s before answering.
//! - `GET /none`: return without setting a response.
//! - anything else: `404`.

// Natively nothing exports the handler (see `export!` below), so its code is
// unused there.
#![cfg_attr(not(target_family = "wasm"), allow(dead_code))]

#[allow(missing_docs, clippy::too_many_arguments)]
mod bindings {
    wit_bindgen::generate!({
        inline: "
            package paddock:fixture-http;
            world handler {
                import paddock:runtime/process@0.1.0;
            }
        ",
        path: ["../../wit"],
        world: "paddock:fixture-http/handler",
        generate_all,
    });
}

use bindings::paddock::runtime::process;
use bindings::paddock::runtime::types::Envelope;
use wasip2::exports::http::incoming_handler::Guest;
use wasip2::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use wasip2::io::streams::{OutputStream, StreamError};

struct Handler;

// Wasm only: native linkers refuse the `:` in the export's symbol.
#[cfg(target_family = "wasm")]
wasip2::http::proxy::export!(Handler);

impl Guest for Handler {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let path_query = request.path_with_query().unwrap_or_default();
        let (path, query) = path_query
            .split_once('?')
            .unwrap_or((path_query.as_str(), ""));
        let param = |name: &str| {
            query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        };
        match path {
            "/hello" if cfg!(feature = "v2") => respond(out, 200, b"hello v2"),
            "/hello" => respond(out, 200, b"hello"),
            "/self" => {
                let p = process::self_();
                let text = format!("{}.{}.{}", p.node, p.id & ((1 << 40) - 1), p.incarnation);
                respond(out, 200, text.as_bytes());
            }
            "/echo" => echo(request, out),
            "/drip" => {
                let chunks: u32 = param("chunks").and_then(|v| v.parse().ok()).unwrap_or(3);
                let ms: u64 = param("ms").and_then(|v| v.parse().ok()).unwrap_or(10);
                let Some((body, stream)) = start(out, 200) else {
                    return;
                };
                for _ in 0..chunks {
                    write(&stream, b"drip\n");
                    sleep_ms(ms);
                }
                finish(body, stream, b"");
            }
            "/call" => {
                let name = param("name").unwrap_or_default();
                let body = param("body").unwrap_or_default();
                let reply = match process::whereis(&name) {
                    None => Err("noproc".to_string()),
                    Some(pid) => {
                        let msg = Envelope {
                            content_type: "text/plain".into(),
                            schema: None,
                            body: body.into_bytes(),
                        };
                        process::call(pid, &msg, 1000)
                            .map(|r| r.body)
                            .map_err(|e| format!("{e:?}"))
                    }
                };
                match reply {
                    Ok(body) => respond(out, 200, &body),
                    Err(e) => respond(out, 502, format!("err:{e}").as_bytes()),
                }
            }
            "/trap" => panic!("trap requested"),
            "/trap-late" => {
                if let Some((_body, stream)) = start(out, 200) {
                    write(&stream, b"partial\n");
                }
                panic!("trap requested after the headers");
            }
            "/spin" =>
            {
                #[allow(clippy::empty_loop)]
                loop {}
            }
            "/hang" => {
                sleep_ms(60_000);
                respond(out, 200, b"late");
            }
            "/none" => {}
            _ => respond(out, 404, b"no such path"),
        }
    }
}

/// Send `status` with an empty header list; the body and its stream.
/// Each is taken once from a new response, so neither fails.
fn start(out: ResponseOutparam, status: u16) -> Option<(OutgoingBody, OutputStream)> {
    let response = OutgoingResponse::new(Fields::new());
    let _ = response.set_status_code(status);
    let body = response.body().ok()?;
    ResponseOutparam::set(out, Ok(response));
    let stream = body.write().ok()?;
    Some((body, stream))
}

fn respond(out: ResponseOutparam, status: u16, bytes: &[u8]) {
    if let Some((body, stream)) = start(out, status) {
        finish(body, stream, bytes);
    }
}

fn finish(body: OutgoingBody, stream: OutputStream, bytes: &[u8]) {
    write(&stream, bytes);
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

fn write(stream: &OutputStream, bytes: &[u8]) {
    // `blocking-write-and-flush` takes at most 4,096 bytes at a time.
    for chunk in bytes.chunks(4096) {
        if stream.blocking_write_and_flush(chunk).is_err() {
            return;
        }
    }
}

/// Stream the request body back, chunk by chunk as it arrives.
fn echo(request: IncomingRequest, out: ResponseOutparam) {
    let Ok(incoming) = request.consume() else {
        return respond(out, 500, b"no body");
    };
    let Ok(input) = incoming.stream() else {
        return respond(out, 500, b"no body stream");
    };
    let Some((body, stream)) = start(out, 200) else {
        return;
    };
    loop {
        match input.blocking_read(16 * 1024) {
            Ok(chunk) => write(&stream, &chunk),
            Err(StreamError::Closed) => break,
            // The host refused the body (over `max_body`): end the response
            // with an error the client sees as a cut stream.
            Err(StreamError::LastOperationFailed(_)) => return,
        }
    }
    finish(body, stream, b"");
}

fn sleep_ms(ms: u64) {
    wasip2::clocks::monotonic_clock::subscribe_duration(ms * 1_000_000).block();
}
