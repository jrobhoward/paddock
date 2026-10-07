//! Arbitrary bytes as a bundle file: opening must fail cleanly or verify.

#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

/// A minisign public key nothing signs with, so every input is either
/// unreadable, corrupt or untrusted, and each check before signature
/// verification gets exercised.
const KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";

fn dir() -> &'static tempfile::TempDir {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap())
}

fuzz_target!(|data: &[u8]| {
    let path = dir().path().join("input.rbp");
    std::fs::write(&path, data).unwrap();
    let _ = paddock_bundle::VerifiedBundle::open(&path, &[KEY.to_string()]);
});
