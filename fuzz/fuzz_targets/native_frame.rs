//! Arbitrary bytes from a native app as a frame's body, through the
//! node's decoders: a hot frame (`bulkhead_native::hot`) or CBOR. Neither
//! panics, and a hot frame that decodes writes back to the same bytes.

#![no_main]

use bulkhead_native::hot::{FromAppHot, is_hot, parse_from_app, write_from_app, write_kv_request};
use bulkhead_native::protocol::{FromApp, decode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|body: &[u8]| {
    if is_hot(body) {
        let mut again = Vec::new();
        match parse_from_app(body) {
            Ok(FromAppHot::Kv(request)) => write_kv_request(&mut again, &request).unwrap(),
            Ok(FromAppHot::Frame(frame)) => write_from_app(&mut again, &frame).unwrap(),
            Err(_) => return,
        }
        assert_eq!(&again[4..], body);
    } else {
        let _ = decode::<FromApp>(body);
    }
});
