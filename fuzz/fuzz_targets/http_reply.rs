//! Arbitrary bodies from a guest through the HTTP reply envelope decoder,
//! which turns an app's reply into a response. It never panics, and a
//! reply it accepts has a valid status code. Needs `--cfg fuzzing`
//! (cargo-fuzz sets it).

#![no_main]

use libfuzzer_sys::fuzz_target;
use paddock_host::http::fuzz_response;

fuzz_target!(|bytes: &[u8]| {
    if let Some(code) = fuzz_response(bytes) {
        assert!((100..=999).contains(&code));
    }
});
