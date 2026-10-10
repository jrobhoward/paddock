//! Arbitrary bytes from a cluster member through the wire decoder: as a
//! hello, then as a packet on a version 1 connection. A hello that decodes
//! agrees on a version both speak or on none; a packet that decodes
//! encodes and decodes back to itself.

#![no_main]

use libfuzzer_sys::fuzz_target;
use paddock_host::transport::wire::{Hello, VERSION, agree, decode, encode, from_cbor};

fuzz_target!(|bytes: &[u8]| {
    if let Ok(theirs) = from_cbor::<Hello>(bytes) {
        let mine = Hello::new(1, 1);
        if let Some(version) = agree(&mine, &theirs) {
            assert!(version >= mine.min_version && version <= mine.max_version);
            assert!(version >= theirs.min_version && version <= theirs.max_version);
        }
    }
    if let Ok(packet) = decode(VERSION, bytes) {
        let again = encode(VERSION, &packet).unwrap();
        assert_eq!(decode(VERSION, &again).unwrap(), packet);
    }
});
