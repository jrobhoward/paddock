//! Opening and verifying bundles end to end.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use common::{Spec, WORLD, rewrite};
use minisign::KeyPair;
use paddock_bundle::{BundleError, ImportLink, OperatorPolicy, VerifiedBundle};
use paddock_manifest::{Capability, ManifestError};
use rustbridge_bundle::{BundleBuilder, Manifest, Platform, builder::compute_sha256};
use serde_json::json;
use tempfile::TempDir;

#[test]
fn open____signed_by_trusted_key____returns_verified_manifest_and_component() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let path = spec.write(&dir);

    let bundle = VerifiedBundle::open(&path, &[spec.public_key()]).unwrap();

    let m = &bundle.manifest;
    assert_eq!(m.app.name, "orders");
    assert_eq!(m.app.version, semver::Version::new(1, 4, 0));
    assert_eq!(m.component.world, WORLD);
    assert_eq!(m.component.path, "lib/wasm32-wasip2/release/orders.wasm");
    assert_eq!(m.provenance.signer_key, spec.public_key());
    assert_eq!(m.spec.registered_names[0].name, "orders");
    assert_eq!(bundle.component, spec.component);
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
fn open____component_swapped_after_signing____refused_corrupt() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let path = spec.write(&dir);
    let tampered = dir.path().join("tampered.rbp");
    rewrite(&path, &tampered, |name, contents| {
        if name.ends_with(".wasm") {
            wat::parse_str("(component)").unwrap()
        } else {
            contents
        }
    });

    let result = VerifiedBundle::open(&tampered, &[spec.public_key()]);

    assert!(matches!(result, Err(BundleError::Corrupt(_))), "{result:?}");
}

#[test]
fn open____native_only_bundle____refused_no_component() {
    let dir = TempDir::new().unwrap();
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let lib = dir.path().join("liborders.so");
    std::fs::write(&lib, b"native").unwrap();
    let path = dir.path().join("native.rbp");
    BundleBuilder::new(Manifest::new("orders", "1.0.0"))
        .add_library(Platform::LinuxX86_64, &lib)
        .unwrap()
        .with_signing_key(keypair.pk.to_base64(), keypair.sk)
        .write(&path)
        .unwrap();

    let result = VerifiedBundle::open(&path, &[keypair.pk.to_base64()]);

    assert!(
        matches!(result, Err(BundleError::NoComponent)),
        "{result:?}"
    );
}

#[test]
fn open____unsupported_world____refused() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        world: "paddock:runtime/app@9.0.0",
        ..Spec::default()
    };
    let path = spec.write(&dir);

    let result = VerifiedBundle::open(&path, &[spec.public_key()]);

    assert!(
        matches!(&result, Err(BundleError::UnsupportedWorld { world, .. }) if world == "paddock:runtime/app@9.0.0"),
        "{result:?}"
    );
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
fn open____native_process_kind____refused_unsupported_kind() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        appserver: Some(json!({ "kind": "native-process" })),
        ..Spec::default()
    };
    let path = spec.write(&dir);

    let err = VerifiedBundle::open(&path, &[spec.public_key()]).unwrap_err();

    assert_eq!(
        err.to_string(),
        "app kind 'native-process' is not supported in Phase 1"
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
fn effective_grant____policy_grants_keyvalue____links_host_implementation() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let bundle = VerifiedBundle::open(spec.write(&dir), &[spec.public_key()]).unwrap();
    let policy =
        OperatorPolicy::from_toml_str("[apps.orders]\ngrant = [\"wasi:keyvalue\"]\n").unwrap();

    let grant = bundle.effective_grant(&policy).unwrap();

    assert_eq!(
        grant.imports["wasi:keyvalue/store@0.2.0-draft"],
        ImportLink::Granted(Capability::KeyValue)
    );
    assert_eq!(
        grant.imports["paddock:runtime/process@0.1.0"],
        ImportLink::Implicit
    );
}

#[test]
fn effective_grant____policy_silent____links_deny_stubs() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let bundle = VerifiedBundle::open(spec.write(&dir), &[spec.public_key()]).unwrap();

    let grant = bundle.effective_grant(&OperatorPolicy::default()).unwrap();

    assert!(grant.denied.contains(&Capability::KeyValue));
}

#[test]
fn effective_grant____import_not_declared____deploy_refused() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        appserver: Some(json!({})),
        ..Spec::default()
    };
    let bundle = VerifiedBundle::open(spec.write(&dir), &[spec.public_key()]).unwrap();

    let result = bundle.effective_grant(&OperatorPolicy::default());

    assert!(
        matches!(&result, Err(BundleError::ImportsRefused { undeclared, .. }) if undeclared.len() == 1),
        "{result:?}"
    );
}

#[test]
fn effective_grant____component_is_not_wasm____invalid_component() {
    let dir = TempDir::new().unwrap();
    let spec = Spec {
        component: b"not a component".to_vec(),
        ..Spec::default()
    };
    let bundle = VerifiedBundle::open(spec.write(&dir), &[spec.public_key()]).unwrap();

    let result = bundle.effective_grant(&OperatorPolicy::default());

    assert!(
        matches!(result, Err(BundleError::InvalidComponent(_))),
        "{result:?}"
    );
}

#[test]
fn open____checksum_recorded____matches_component_bytes() {
    let dir = TempDir::new().unwrap();
    let spec = Spec::default();
    let path = spec.write(&dir);

    let bundle = VerifiedBundle::open(&path, &[spec.public_key()]).unwrap();

    assert_eq!(
        bundle.manifest.component.checksum,
        format!("sha256:{}", compute_sha256(&spec.component))
    );
}
