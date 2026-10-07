//! Arbitrary bodies and content types through hash routing's key reader,
//! which runs on the host for every routed message. Needs `--cfg fuzzing`
//! (cargo-fuzz sets it).

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use paddock_host::Envelope;

#[derive(Debug, Arbitrary)]
struct Input {
    json: bool,
    charset: bool,
    field: String,
    body: Vec<u8>,
}

fuzz_target!(|input: Input| {
    let mut content_type = if input.json { "application/json" } else { "application/cbor" }.to_string();
    if input.charset {
        content_type.push_str("; charset=utf-8");
    }
    let _ = paddock_host::routing_key(&Envelope::new(content_type, input.body), &input.field);
});
