#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn PublicKey____text_form____round_trips_and_rejects_junk() {
    let (_, public) = generate_key().unwrap();

    let text = public.to_string();
    let back: PublicKey = text.parse().unwrap();

    assert!(text.starts_with("ed25519:"), "{text}");
    assert_eq!(back, public);
    assert!("ed25519:AAAA".parse::<PublicKey>().is_err());
    assert!("rsa:AAAA".parse::<PublicKey>().is_err());
}

#[test]
fn load____key_and_peers_files____own_key_and_peer_keys() {
    let dir = tempfile::tempdir().unwrap();
    let (private, public) = generate_key().unwrap();
    let (_, other) = generate_key().unwrap();
    let key = dir.path().join("node.key");
    let peers = dir.path().join("peers.toml");
    std::fs::write(&key, private).unwrap();
    std::fs::write(&peers, format!("[peers]\n2 = [\"{other}\"]\n")).unwrap();

    let loaded = NodeIdentity {
        key: key.clone(),
        peers,
    }
    .load()
    .unwrap();

    assert_eq!(public_key_of(&key).unwrap(), public);
    assert!(loaded.names(2, &other));
    assert!(!loaded.names(2, &public));
    assert!(!loaded.names(3, &other));
}

#[test]
fn parse_peers____bad_id_or_key____refused() {
    assert!(
        parse_peers("[peers]\nx = []\n")
            .unwrap_err()
            .contains("peers.x")
    );
    assert!(
        parse_peers("[peers]\n1 = [\"ed25519:zz\"]\n")
            .unwrap_err()
            .contains("peers.1")
    );
    assert!(parse_peers("[peer]\n").is_err());
}
