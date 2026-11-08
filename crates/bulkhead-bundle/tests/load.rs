//! Opening and verifying bundles end to end.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use bulkhead_bundle::manifest::{Capability, ManifestError};
use bulkhead_bundle::{BundleError, OperatorPolicy, VerifiedBundle};
use common::{Spec, append, elf, rewrite};
use minisign::KeyPair;
use rstest::rstest;
use rustbridge_bundle::{BundleBuilder, Manifest, Platform, builder::compute_sha256};
use serde_json::json;
use tempfile::TempDir;

#[test]
fn open____signed_by_trusted_key____returns_verified_manifest_and_program() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let path = spec.write(&dir);

    let bundle = VerifiedBundle::open(&path, &[spec.public_key()]).unwrap();

    let m = &bundle.manifest;
    assert_eq!(m.app.name, "orders");
    assert_eq!(m.app.version, semver::Version::new(1, 4, 0));
    assert!(
        m.program.path.ends_with("/release/orders"),
        "{}",
        m.program.path
    );
    assert_eq!(m.provenance.signer_key, spec.public_key());
    assert_eq!(m.spec.registered_names[0].name, "orders");
    assert_eq!(bundle.program, spec.program);
}

#[test]
fn open____second_trusted_key_signed____records_that_key() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let path = spec.write(&dir);
    let other = KeyPair::generate_unencrypted_keypair()
        .unwrap()
        .pk
        .to_base64();

    let bundle = VerifiedBundle::open(&path, &[other, spec.public_key()]).unwrap();

    assert_eq!(bundle.manifest.provenance.signer_key, spec.public_key());
}

#[test]
fn open____signed_by_untrusted_key____refused_untrusted() {
    let dir = TempDir::new().unwrap();
    let path = Spec::default().write(&dir);
    let operator = KeyPair::generate_unencrypted_keypair()
        .unwrap()
        .pk
        .to_base64();

    let result = VerifiedBundle::open(&path, &[operator]);

    assert!(
        matches!(result, Err(BundleError::Untrusted(_))),
        "{result:?}"
    );
}

#[test]
fn open____unsigned____refused_untrusted() {
    let dir = TempDir::new().unwrap();
    let operator = KeyPair::generate_unencrypted_keypair()
        .unwrap()
        .pk
        .to_base64();
    let path = Spec {
        signer: None,
        ..Spec::default()
    }
    .write(&dir);

    let err = VerifiedBundle::open(&path, &[operator]).unwrap_err();

    assert!(err.to_string().contains("not signed"), "{err}");
}

#[test]
fn open____no_trusted_keys_configured____refused_untrusted() {
    let dir = TempDir::new().unwrap();
    let path = Spec::default().write(&dir);

    let result = VerifiedBundle::open(&path, &[]);

    assert!(
        matches!(result, Err(BundleError::Untrusted(_))),
        "{result:?}"
    );
}

#[test]
fn open____manifest_edited_after_signing____refused_untrusted() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let path = spec.write(&dir);
    let tampered = dir.path().join("tampered.rbp");
    rewrite(&path, &tampered, |name, contents| {
        if name == "manifest.json" {
            String::from_utf8(contents)
                .unwrap()
                .replace("32MiB", "4GiB")
                .into_bytes()
        } else {
            contents
        }
    });

    let result = VerifiedBundle::open(&tampered, &[spec.public_key()]);

    assert!(
        matches!(result, Err(BundleError::Untrusted(_))),
        "{result:?}"
    );
}

#[test]
fn open____program_swapped_after_signing____refused_corrupt() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let path = spec.write(&dir);
    let tampered = dir.path().join("tampered.rbp");
    rewrite(&path, &tampered, |name, contents| {
        if name.ends_with("/release/orders") {
            vec![0; 80]
        } else {
            contents
        }
    });

    let result = VerifiedBundle::open(&tampered, &[spec.public_key()]);

    assert!(matches!(result, Err(BundleError::Corrupt(_))), "{result:?}");
}

/// The other native platform than this machine's.
fn other_platform() -> Platform {
    if Platform::current() == Some(Platform::LinuxAarch64) {
        Platform::LinuxX86_64
    } else {
        Platform::LinuxAarch64
    }
}

/// A signed bundle of kind `native-process` with a program for each of
/// `programs`' platforms.
fn native_bundle(
    dir: &TempDir,
    keypair: KeyPair,
    programs: &[(Platform, Vec<u8>)],
) -> std::path::PathBuf {
    let path = dir.path().join("native.rbp");
    let mut builder = BundleBuilder::new(Manifest::new("orders", "1.0.0"));
    for (platform, program) in programs {
        let lib = dir.path().join(platform.as_str()).join("liborders.so");
        std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
        std::fs::write(&lib, program).unwrap();
        builder = builder.add_library(*platform, &lib).unwrap();
    }
    builder
        .with_extension(
            "appserver",
            json!({ "kind": "native-process", "capabilities": { "wasi:logging": {} } }),
        )
        .with_signing_key(keypair.pk.to_base64(), keypair.sk)
        .write(&path)
        .unwrap();
    path
}

#[test]
fn open____no_appserver_section____refused() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        appserver: None,
        ..Spec::default()
    };
    let path = spec.write(&dir);

    let result = VerifiedBundle::open(&path, &[spec.public_key()]);

    assert!(
        matches!(result, Err(BundleError::NoAppSection)),
        "{result:?}"
    );
}

#[test]
fn open____invalid_appserver_section____error_path_rooted_at_section() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        appserver: Some(json!({ "limits": { "memory_max": 0 } })),
        ..Spec::default()
    };
    let path = spec.write(&dir);

    let err = VerifiedBundle::open(&path, &[spec.public_key()]).unwrap_err();

    let BundleError::Manifest(ManifestError::Invalid(errors)) = err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert_eq!(errors[0].path, "appserver.limits.memory_max");
}

#[test]
fn open____malformed_appserver_section____parse_path_rooted_at_section() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        appserver: Some(json!({ "limits": { "memory_max": "lots" } })),
        ..Spec::default()
    };
    let path = spec.write(&dir);

    let err = VerifiedBundle::open(&path, &[spec.public_key()]).unwrap_err();

    let BundleError::Manifest(ManifestError::Parse { path, .. }) = err else {
        panic!("expected Parse, got {err:?}");
    };
    assert_eq!(path, "appserver.limits.memory_max");
}

#[test]
fn open____native_process_for_this_platform____program_and_declared_grant() {
    let dir = TempDir::new().unwrap();
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let key = keypair.pk.to_base64();
    let this = Platform::current().unwrap();
    let path = native_bundle(&dir, keypair, &[(this, elf(this))]);

    let bundle = VerifiedBundle::open(&path, &[key]).unwrap();
    let grant = bundle.effective_grant(
        &OperatorPolicy::from_toml_str("[apps.orders]\ngrant = [\"wasi:logging\"]\n").unwrap(),
    );

    assert_eq!(bundle.program, elf(this));
    assert!(grant.granted.contains(&Capability::Logging));
}

#[test]
fn open____bundle_of_a_component____refused_saying_so() {
    let dir = TempDir::new().unwrap();
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let key = keypair.pk.to_base64();
    let wasm = dir.path().join("orders.wasm");
    std::fs::write(&wasm, b"\0asm\x0d\0\x01\0").unwrap();
    let path = dir.path().join("component.rbp");
    BundleBuilder::new(Manifest::new("orders", "1.0.0"))
        .add_component(Platform::Wasm32Wasip2, &wasm, "bulkhead:runtime/app@0.1.0")
        .unwrap()
        .with_extension("appserver", json!({}))
        .with_signing_key(key.clone(), keypair.sk)
        .write(&path)
        .unwrap();

    let err = VerifiedBundle::open(&path, &[key]).unwrap_err();

    assert!(matches!(err, BundleError::Component), "{err:?}");
}

#[test]
fn open____native_programs_for_both_platforms____this_platforms_program() {
    let dir = TempDir::new().unwrap();
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let key = keypair.pk.to_base64();
    let (this, other) = (Platform::current().unwrap(), other_platform());
    let path = native_bundle(&dir, keypair, &[(this, elf(this)), (other, elf(other))]);

    let bundle = VerifiedBundle::open(&path, &[key]).unwrap();

    assert_eq!(bundle.program, elf(this));
}

#[test]
fn open_for_platform____native_programs_for_both____that_platforms_program() {
    let dir = TempDir::new().unwrap();
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let key = keypair.pk.to_base64();
    let (this, other) = (Platform::current().unwrap(), other_platform());
    let path = native_bundle(&dir, keypair, &[(this, elf(this)), (other, elf(other))]);

    let bundle = VerifiedBundle::open_for_platform(&path, &[key], other).unwrap();

    assert_eq!(bundle.program, elf(other));
}

#[test]
fn open____native_program_for_the_other_platform_only____refused_naming_both() {
    let dir = TempDir::new().unwrap();
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let key = keypair.pk.to_base64();
    let other = other_platform();
    let path = native_bundle(&dir, keypair, &[(other, elf(other))]);

    let err = VerifiedBundle::open(&path, &[key]).unwrap_err();

    let BundleError::NoNativeVariant { platform, present } = &err else {
        panic!("expected NoNativeVariant, got {err:?}");
    };
    assert_eq!(platform, Platform::current().unwrap().as_str());
    assert_eq!(present, other.as_str());
    assert!(err.to_string().contains("--program"), "{err}");
}

#[test]
fn open____native_program_for_the_wrong_architecture____refused_wrong_program() {
    let dir = TempDir::new().unwrap();
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let key = keypair.pk.to_base64();
    let this = Platform::current().unwrap();
    let path = native_bundle(&dir, keypair, &[(this, elf(other_platform()))]);

    let err = VerifiedBundle::open(&path, &[key]).unwrap_err();

    assert!(
        matches!(err, BundleError::WrongProgram { ref platform, .. } if platform == this.as_str()),
        "{err:?}"
    );
}

#[test]
fn open____plugin_version_not_semver____error_at_plugin_version() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        version: "one",
        ..Spec::default()
    };
    let path = spec.write(&dir);

    let err = VerifiedBundle::open(&path, &[spec.public_key()]).unwrap_err();

    let BundleError::Manifest(ManifestError::Parse { path, .. }) = err else {
        panic!("expected Parse, got {err:?}");
    };
    assert_eq!(path, "plugin.version");
}

#[test]
fn open____invalid_app_name____error_at_plugin_name() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        name: "Orders",
        ..Spec::default()
    };
    let path = spec.write(&dir);

    let err = VerifiedBundle::open(&path, &[spec.public_key()]).unwrap_err();

    let BundleError::Manifest(ManifestError::Invalid(errors)) = err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert_eq!(errors[0].path, "plugin.name");
}

#[test]
fn effective_grant____policy_grants_keyvalue____granted() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let bundle = VerifiedBundle::open(spec.write(&dir), &[spec.public_key()]).unwrap();
    let policy =
        OperatorPolicy::from_toml_str("[apps.orders]\ngrant = [\"wasi:keyvalue\"]\n").unwrap();

    let grant = bundle.effective_grant(&policy);

    assert!(grant.granted.contains(&Capability::KeyValue));
}

#[test]
fn effective_grant____policy_silent____denied() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let bundle = VerifiedBundle::open(spec.write(&dir), &[spec.public_key()]).unwrap();

    let grant = bundle.effective_grant(&OperatorPolicy::default());

    assert!(grant.denied.contains(&Capability::KeyValue));
}

#[test]
fn open____checksum_recorded____matches_program_bytes() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let path = spec.write(&dir);

    let bundle = VerifiedBundle::open(&path, &[spec.public_key()]).unwrap();

    assert_eq!(
        bundle.manifest.program.checksum,
        format!("sha256:{}", compute_sha256(&spec.program))
    );
}

#[test]
fn open____checksummed_priv_files____read_and_extracted() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        files: vec![
            ("priv/config/defaults.toml", b"x = 1".to_vec()),
            ("priv/README", b"hi".to_vec()),
        ],
        ..Spec::default()
    };
    let bundle = VerifiedBundle::open(spec.write(&dir), &[spec.public_key()]).unwrap();
    let out = dir.path().join("extracted");

    bundle.extract_priv(&out).unwrap();

    let names: Vec<&str> = bundle.priv_files.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(names, vec!["README", "config/defaults.toml"]);
    assert_eq!(
        std::fs::read_to_string(out.join("config/defaults.toml")).unwrap(),
        "x = 1"
    );
}

#[test]
fn open____priv_file_without_checksum____refused_untrusted() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        raw_files: vec![("priv/sneaky.txt", b"x".to_vec())],
        ..Spec::default()
    };

    let err = VerifiedBundle::open(spec.write(&dir), &[spec.public_key()]).unwrap_err();

    assert!(
        matches!(&err, BundleError::Untrusted(why) if why.contains("priv/sneaky.txt")),
        "{err:?}"
    );
}

#[test]
fn open____priv_file_changed_after_signing____refused_corrupt() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        files: vec![("priv/data.txt", b"original".to_vec())],
        ..Spec::default()
    };
    let path = spec.write(&dir);
    let tampered = dir.path().join("tampered.rbp");
    rewrite(&path, &tampered, |name, contents| {
        if name == "priv/data.txt" {
            b"changed".to_vec()
        } else {
            contents
        }
    });

    let result = VerifiedBundle::open(&tampered, &[spec.public_key()]);

    assert!(matches!(result, Err(BundleError::Corrupt(_))), "{result:?}");
}

#[rstest]
#[case("priv/../escaped.txt")]
#[case("priv/config/../../escaped.txt")]
#[case("priv//etc/escaped.txt")]
#[case("priv/./escaped.txt")]
fn open____priv_path_leaving_the_directory____refused_unsafe_path(#[case] path: &str) {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let bundle = dir.path().join("evil.rbp");
    append(&spec.write(&dir), &bundle, path, b"x");

    let result = VerifiedBundle::open(&bundle, &[spec.public_key()]);

    assert!(
        matches!(&result, Err(BundleError::UnsafePath(_))),
        "{path}: {result:?}"
    );
}

#[test]
fn extract_priv____unsafe_path_in_priv_files____refused_nothing_written() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let mut bundle = VerifiedBundle::open(spec.write(&dir), &[spec.public_key()]).unwrap();
    bundle.priv_files = vec![
        ("ok.txt".to_string(), b"x".to_vec()),
        ("../escaped.txt".to_string(), b"x".to_vec()),
    ];
    let out = dir.path().join("extracted");

    let result = bundle.extract_priv(&out);

    assert!(
        matches!(&result, Err(BundleError::UnsafePath(p)) if p == "../escaped.txt"),
        "{result:?}"
    );
    assert!(!out.exists());
    assert!(!dir.path().join("escaped.txt").exists());
}
