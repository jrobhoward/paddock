#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn component(imports: &[&str]) -> Vec<u8> {
    let body: String = imports
        .iter()
        .map(|i| format!("(import \"{i}\" (instance))"))
        .collect();
    wat::parse_str(format!("(component {body})")).unwrap()
}

struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new(manifest: &str, imports: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("paddock.toml"), manifest).unwrap();
        std::fs::write(dir.path().join("app.wasm"), component(imports)).unwrap();
        keygen(&dir.path().join("signer"), true).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn bundle(&self, priv_dir: Option<&Path>, skip: bool) -> anyhow::Result<Bundled> {
        let (m, c, k, o) = (
            self.path("paddock.toml"),
            self.path("app.wasm"),
            self.path("signer.key"),
            self.path("out.rbp"),
        );
        bundle(&BundleRequest {
            manifest: &m,
            component: &c,
            key: &k,
            priv_dir,
            out: Some(&o),
            skip_import_check: skip,
        })
    }
}

const KV_APP: &str = r#"
[app]
name = "orders"
version = "1.2.0"
description = "Orders"

[appserver.capabilities."wasi:keyvalue".buckets.orders]
durability = "disk"
"#;

#[test]
fn bundle____declared_imports____written_signed_and_verifiable() {
    let s = Setup::new(
        KV_APP,
        &[
            "paddock:runtime/process@0.1.0",
            "wasi:keyvalue/store@0.2.0-draft",
        ],
    );

    let out = s.bundle(None, false).unwrap();

    assert_eq!(
        (out.name.as_str(), out.version.as_str()),
        ("orders", "1.2.0")
    );
    let pubkey = std::fs::read_to_string(s.path("signer.pub")).unwrap();
    assert_eq!(pubkey.lines().nth(1).unwrap().trim(), out.public_key);
    let bundle =
        paddock_bundle::VerifiedBundle::open(&out.path, std::slice::from_ref(&out.public_key))
            .unwrap();
    assert_eq!(bundle.manifest.app.description.as_deref(), Some("Orders"));
    assert!(bundle.manifest.spec.capabilities.keyvalue.is_some());
}

#[test]
fn bundle____undeclared_import____refused_naming_it() {
    let s = Setup::new(
        "[app]\nname = \"x\"\nversion = \"1.0.0\"\n",
        &["wasi:keyvalue/store@0.2.0-draft"],
    );

    let err = s.bundle(None, false).unwrap_err();

    assert!(
        err.to_string()
            .contains("wasi:keyvalue/store@0.2.0-draft (needs wasi:keyvalue)"),
        "{err}"
    );
    assert!(!s.path("out.rbp").exists());
}

#[test]
fn bundle____skip_import_check____written_anyway() {
    let s = Setup::new(
        "[app]\nname = \"x\"\nversion = \"1.0.0\"\n",
        &["wasi:keyvalue/store@0.2.0-draft"],
    );

    let out = s.bundle(None, true);

    assert!(out.is_ok(), "{out:?}");
}

#[test]
fn bundle____invalid_appserver____error_names_the_field() {
    let s = Setup::new(
        "[app]\nname = \"x\"\nversion = \"1.0.0\"\n[appserver.limits]\nmemory_max = \"lots\"\n",
        &[],
    );

    let err = s.bundle(None, false).unwrap_err();

    assert!(err.to_string().contains("limits.memory_max"), "{err}");
}

#[test]
fn bundle____bad_name_or_version____refused() {
    let bad_name = Setup::new("[app]\nname = \"Orders\"\nversion = \"1.0.0\"\n", &[]);
    let bad_version = Setup::new("[app]\nname = \"x\"\nversion = \"one\"\n", &[]);

    assert!(bad_name.bundle(None, false).is_err());
    assert!(bad_version.bundle(None, false).is_err());
}

#[test]
fn bundle____priv_dir____files_checksummed_under_priv() {
    let s = Setup::new("[app]\nname = \"x\"\nversion = \"1.0.0\"\n", &[]);
    let priv_dir = s.path("priv");
    std::fs::create_dir_all(priv_dir.join("conf")).unwrap();
    std::fs::write(priv_dir.join("README"), "hi").unwrap();
    std::fs::write(priv_dir.join("conf/app.toml"), "a = 1").unwrap();

    let out = s.bundle(Some(&priv_dir), false).unwrap();

    let bundle =
        paddock_bundle::VerifiedBundle::open(&out.path, std::slice::from_ref(&out.public_key))
            .unwrap();
    let names: Vec<&str> = bundle.priv_files.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(names, vec!["README", "conf/app.toml"]);
    assert_eq!(out.priv_files, 2);
}

#[test]
fn keygen____existing_files____refused() {
    let dir = tempfile::tempdir().unwrap();
    keygen(&dir.path().join("k"), true).unwrap();

    let again = keygen(&dir.path().join("k"), true);

    assert!(again.is_err());
}

#[cfg(unix)]
#[test]
fn bundle____key_readable_by_others____refused() {
    use std::os::unix::fs::PermissionsExt;
    let setup = Setup::new(KV_APP, &["wasi:keyvalue/store@0.2.0-draft"]);
    let key = setup.path("signer.key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();

    let result = setup.bundle(None, false);

    let error = format!("{:#}", result.err().unwrap());
    assert!(error.contains("chmod 600"), "{error}");
    assert!(!setup.path("out.rbp").exists());
}

#[test]
fn bundle____native_process_app____program_as_this_platforms_variant() {
    let s = Setup::new(
        "[app]\nname = \"vendor\"\nversion = \"1.0.0\"\n\n[appserver]\nkind = \"native-process\"\n\n[appserver.native]\nprogram = \"executable\"\n",
        &[],
    );
    std::fs::write(s.path("app.wasm"), b"\x7fELF not a component").unwrap();

    let out = s.bundle(None, false).unwrap();

    let bundle =
        paddock_bundle::VerifiedBundle::open(&out.path, std::slice::from_ref(&out.public_key))
            .unwrap();
    assert!(out.imports.is_empty());
    assert_eq!(bundle.component, b"\x7fELF not a component");
}
