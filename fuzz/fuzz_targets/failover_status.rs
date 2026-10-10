//! Arbitrary heartbeat payloads from the partner through the failover
//! status decoder. A payload that decodes encodes to 26 bytes that decode
//! to the same status. Needs `--cfg fuzzing` (cargo-fuzz sets it).

#![no_main]

use libfuzzer_sys::fuzz_target;
use paddock_host::failover::fuzz_status;

fuzz_target!(|bytes: &[u8]| {
    if let Some(encoded) = fuzz_status(bytes) {
        assert_eq!(encoded.len(), 26);
        assert_eq!(fuzz_status(&encoded).as_deref(), Some(encoded.as_slice()));
    }
});
