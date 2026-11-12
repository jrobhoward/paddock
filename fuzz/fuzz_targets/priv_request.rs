//! Arbitrary bytes from a node as a request to the privileged helper
//! (`bulkhead_priv_wire`), which parses them as root: the parser never
//! panics, and a request that decodes encodes back to the same bytes, so
//! nothing it accepts is read two ways.

#![no_main]

use bulkhead_priv_wire::{Reply, Request};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Some(request) = Request::decode(bytes) {
        assert_eq!(request.encode(), bytes);
    }
    if let Some(reply) = Reply::decode(bytes) {
        assert_eq!(reply.encode(), bytes);
    }
});
