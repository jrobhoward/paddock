//! Arbitrary snapshot bytes from a guest through the reader behind
//! `paddock state`. It never panics, always names a format, and its answer
//! serializes as JSON for the admin API. Needs `--cfg fuzzing` (cargo-fuzz
//! sets it).

#![no_main]

use libfuzzer_sys::fuzz_target;
use paddock::node::describe;

fuzz_target!(|bytes: &[u8]| {
    let view = describe("1.2.3", bytes);
    assert_eq!(view.bytes, bytes.len());
    assert!(!view.format.is_empty());
    serde_json::to_string(&view).unwrap();
});
