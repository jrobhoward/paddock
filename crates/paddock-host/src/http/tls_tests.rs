#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::Path;

use super::*;

/// A self-signed certificate for `hosts`, written as `<name>.pem` and
/// `<name>.key` in `dir`.
fn write_cert(dir: &Path, name: &str, hosts: &[&str]) -> TlsCertificate {
    let made = rcgen::generate_simple_self_signed(
        hosts.iter().map(ToString::to_string).collect::<Vec<_>>(),
    )
    .unwrap();
    let cert = dir.join(format!("{name}.pem"));
    let key = dir.join(format!("{name}.key"));
    std::fs::write(&cert, made.cert.pem()).unwrap();
    std::fs::write(&key, made.signing_key.serialize_pem()).unwrap();
    TlsCertificate {
        cert,
        key,
        hosts: hosts.iter().map(ToString::to_string).collect(),
    }
}

fn leaf(k: &CertifiedKey) -> Vec<u8> {
    k.cert[0].to_vec()
}

#[test]
fn pick____exact_wildcard_and_unknown_names____matching_certificate_or_default() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_cert(dir.path(), "a", &["a.example"]);
    let wild = write_cert(dir.path(), "wild", &["*.shop.example"]);
    let loaded = load(&[a, wild]).unwrap();

    let exact = loaded.pick(Some("A.Example"));
    let via_wildcard = loaded.pick(Some("eu.shop.example"));
    let unknown = loaded.pick(Some("other.example"));
    let none = loaded.pick(None);

    let a_leaf = leaf(&loaded.by_host["a.example"]);
    let wild_leaf = leaf(&loaded.by_host["*.shop.example"]);
    assert_eq!(leaf(&exact), a_leaf);
    assert_eq!(leaf(&via_wildcard), wild_leaf);
    assert_eq!(leaf(&unknown), a_leaf);
    assert_eq!(leaf(&none), a_leaf);
}

#[test]
fn load____key_of_another_certificate____refused() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_cert(dir.path(), "a", &["a.example"]);
    let b = write_cert(dir.path(), "b", &["b.example"]);
    let mismatched = TlsCertificate { key: b.key, ..a };

    let err = load(&[mismatched]).unwrap_err().to_string();

    assert!(err.contains("TLS"), "{err}");
}

#[test]
fn reload_if_changed____rewritten_files____new_set_and_broken_files_keep_the_old() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_cert(dir.path(), "a", &["a.example"]);
    let tls = IngressTls::load(vec![a.clone()]).unwrap();
    let before = leaf(&tls.inner.current.read().unwrap().default);

    let unchanged = tls.reload_if_changed().unwrap();
    // A new certificate in the same files (mtime granularity can be coarse).
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_cert(dir.path(), "a", &["a.example"]);
    let changed = tls.reload_if_changed().unwrap();
    let after = leaf(&tls.inner.current.read().unwrap().default);
    std::fs::write(&a.cert, "not a certificate").unwrap();
    let broken = tls.reload();
    let kept = leaf(&tls.inner.current.read().unwrap().default);

    assert!(!unchanged);
    assert!(changed);
    assert_ne!(before, after);
    assert!(broken.is_err());
    assert_eq!(kept, after);
}
