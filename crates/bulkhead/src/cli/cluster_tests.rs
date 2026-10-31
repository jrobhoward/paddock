#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn keygen____new_file____pubkey_reads_the_same_public_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys/node.key");

    let public = keygen(&path).unwrap();
    let again = pubkey(&path).unwrap();

    assert_eq!(public, again);
    assert!(public.to_string().starts_with("ed25519:"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn keygen____existing_file____refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.key");
    keygen(&path).unwrap();

    assert!(keygen(&path).is_err());
}
