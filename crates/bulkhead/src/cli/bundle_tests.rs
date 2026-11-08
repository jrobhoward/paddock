#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use rstest::rstest;

struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new(manifest: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bulkhead.toml"), manifest).unwrap();
        std::fs::write(dir.path().join("app"), elf(this_platform())).unwrap();
        keygen(&dir.path().join("signer"), true).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn bundle(&self, priv_dir: Option<&Path>) -> anyhow::Result<Bundled> {
        let (m, c, k, o) = (
            self.path("bulkhead.toml"),
            self.path("app"),
            self.path("signer.key"),
            self.path("out.rbp"),
        );
        bundle(&BundleRequest {
            manifest: &m,
            program: Some(&c),
            programs: &[],
            key: &k,
            priv_dir,
            out: Some(&o),
        })
    }
}

const KV_APP: &str = r#"
[app]
name = "orders"
version = "1.2.0"
description = "Orders"

[appserver.capabilities."bulkhead:keyvalue".buckets.orders]
durability = "disk"
"#;

#[test]
fn bundle____program____written_signed_and_verifiable() {
    let s = Setup::new(KV_APP);

    let out = s.bundle(None).unwrap();

    assert_eq!(
        (out.name.as_str(), out.version.as_str()),
        ("orders", "1.2.0")
    );
    let pubkey = std::fs::read_to_string(s.path("signer.pub")).unwrap();
    assert_eq!(pubkey.lines().nth(1).unwrap().trim(), out.public_key);
    let bundle =
        bulkhead_bundle::VerifiedBundle::open(&out.path, std::slice::from_ref(&out.public_key))
            .unwrap();
    assert_eq!(bundle.manifest.app.description.as_deref(), Some("Orders"));
    assert!(bundle.manifest.spec.capabilities.keyvalue.is_some());
}

#[test]
fn bundle____invalid_appserver____error_names_the_field() {
    let s = Setup::new(
        "[app]\nname = \"x\"\nversion = \"1.0.0\"\n[appserver.limits]\nmemory_max = \"lots\"\n",
    );

    let err = s.bundle(None).unwrap_err();

    assert!(err.to_string().contains("limits.memory_max"), "{err}");
}

#[test]
fn bundle____bad_name_or_version____refused() {
    let bad_name = Setup::new("[app]\nname = \"Orders\"\nversion = \"1.0.0\"\n");
    let bad_version = Setup::new("[app]\nname = \"x\"\nversion = \"one\"\n");

    assert!(bad_name.bundle(None).is_err());
    assert!(bad_version.bundle(None).is_err());
}

#[test]
fn bundle____priv_dir____files_checksummed_under_priv() {
    let s = Setup::new("[app]\nname = \"x\"\nversion = \"1.0.0\"\n");
    let priv_dir = s.path("priv");
    std::fs::create_dir_all(priv_dir.join("conf")).unwrap();
    std::fs::write(priv_dir.join("README"), "hi").unwrap();
    std::fs::write(priv_dir.join("conf/app.toml"), "a = 1").unwrap();

    let out = s.bundle(Some(&priv_dir)).unwrap();

    let bundle =
        bulkhead_bundle::VerifiedBundle::open(&out.path, std::slice::from_ref(&out.public_key))
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
    let setup = Setup::new(KV_APP);
    let key = setup.path("signer.key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();

    let result = setup.bundle(None);

    let error = format!("{:#}", result.err().unwrap());
    assert!(error.contains("chmod 600"), "{error}");
    assert!(!setup.path("out.rbp").exists());
}

const NATIVE_APP: &str = "[app]\nname = \"vendor\"\nversion = \"1.0.0\"\n\n[appserver]\nkind = \"native-process\"\n\n[appserver.native]\nprogram = \"executable\"\n";

/// The start of an ELF file for `platform`'s architecture.
fn elf(platform: Platform) -> Vec<u8> {
    let machine: u16 = if platform == Platform::LinuxAarch64 {
        183
    } else {
        62
    };
    let mut header = vec![0u8; 64];
    header[..4].copy_from_slice(b"\x7fELF");
    header[4] = 2;
    header[5] = 1;
    header[18..20].copy_from_slice(&machine.to_le_bytes());
    header
}

fn this_platform() -> Platform {
    Platform::current().unwrap()
}

fn other_platform() -> Platform {
    if this_platform() == Platform::LinuxAarch64 {
        Platform::LinuxX86_64
    } else {
        Platform::LinuxAarch64
    }
}

impl Setup {
    /// A native app's program for `platform`, written as `name`.
    fn program(&self, name: &str, platform: Platform) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, elf(platform)).unwrap();
        path
    }

    fn bundle_programs(
        &self,
        program: Option<&Path>,
        programs: &[(Platform, PathBuf)],
    ) -> anyhow::Result<Bundled> {
        let (m, k, o) = (
            self.path("bulkhead.toml"),
            self.path("signer.key"),
            self.path("out.rbp"),
        );
        bundle(&BundleRequest {
            manifest: &m,
            program,
            programs,
            key: &k,
            priv_dir: None,
            out: Some(&o),
        })
    }

    fn open(&self, out: &Bundled) -> bulkhead_bundle::VerifiedBundle {
        bulkhead_bundle::VerifiedBundle::open(&out.path, std::slice::from_ref(&out.public_key))
            .unwrap()
    }
}

#[test]
fn bundle____native_process_app____program_as_this_platforms_variant() {
    let s = Setup::new(NATIVE_APP);
    let program = s.program("app", this_platform());

    let out = s.bundle_programs(Some(&program), &[]).unwrap();

    assert_eq!(out.platforms, [this_platform().as_str()]);
    assert_eq!(s.open(&out).program, elf(this_platform()));
}

#[test]
fn bundle____program_per_platform____both_variants_and_the_node_runs_its_own() {
    let s = Setup::new(NATIVE_APP);
    let programs = [
        (
            Platform::LinuxX86_64,
            s.program("app-x86_64", Platform::LinuxX86_64),
        ),
        (
            Platform::LinuxAarch64,
            s.program("app-aarch64", Platform::LinuxAarch64),
        ),
    ];

    let out = s.bundle_programs(None, &programs).unwrap();

    assert_eq!(out.platforms, ["linux-x86_64", "linux-aarch64"]);
    assert_eq!(s.open(&out).program, elf(this_platform()));
}

#[test]
fn bundle____other_platforms_program_only____written_for_that_platform() {
    let s = Setup::new(NATIVE_APP);
    let program = s.program("app", other_platform());

    let out = s
        .bundle_programs(None, &[(other_platform(), program)])
        .unwrap();

    let opened = bulkhead_bundle::VerifiedBundle::open_for_platform(
        &out.path,
        std::slice::from_ref(&out.public_key),
        other_platform(),
    )
    .unwrap();
    assert_eq!(opened.program, elf(other_platform()));
}

#[test]
fn bundle____program_for_another_architecture____refused_naming_the_file() {
    let s = Setup::new(NATIVE_APP);
    let wrong = s.program("app", other_platform());

    let result = s.bundle_programs(None, &[(this_platform(), wrong.clone())]);

    let error = format!("{:#}", result.err().unwrap());
    assert!(error.contains(&wrong.display().to_string()), "{error}");
    assert!(
        error.contains(&format!(
            "the program for {} is an ELF file for",
            this_platform().as_str()
        )),
        "{error}"
    );
    assert!(!s.path("out.rbp").exists());
}

#[test]
fn bundle____program_not_an_elf_file____refused() {
    let s = Setup::new(NATIVE_APP);
    let script = s.path("app.sh");
    std::fs::write(&script, "#!/bin/sh\n").unwrap();

    let result = s.bundle_programs(Some(&script), &[]);

    let error = format!("{:#}", result.err().unwrap());
    assert!(error.contains("not an ELF file"), "{error}");
}

#[test]
fn bundle____argument_and_program_both____refused() {
    let s = Setup::new(NATIVE_APP);
    let program = s.program("app", this_platform());

    let result = s.bundle_programs(Some(&program), &[(this_platform(), program.clone())]);

    let error = format!("{:#}", result.err().unwrap());
    assert!(error.contains("not both"), "{error}");
}

#[test]
fn bundle____native_app_without_a_program____refused() {
    let s = Setup::new(NATIVE_APP);

    let result = s.bundle_programs(None, &[]);

    let error = format!("{:#}", result.err().unwrap());
    assert!(error.contains("needs its program"), "{error}");
}

#[test]
fn bundle____same_platform_twice____refused() {
    let s = Setup::new(NATIVE_APP);
    let program = s.program("app", this_platform());

    let result = s.bundle_programs(
        None,
        &[
            (this_platform(), program.clone()),
            (this_platform(), program),
        ],
    );

    let error = format!("{:#}", result.err().unwrap());
    assert!(error.contains("given twice"), "{error}");
}

#[rstest]
#[case("linux-x86_64=target/app", Platform::LinuxX86_64, "target/app")]
#[case("linux-aarch64=/a=b", Platform::LinuxAarch64, "/a=b")]
fn parse_program____platform_and_path____parsed(
    #[case] value: &str,
    #[case] platform: Platform,
    #[case] path: &str,
) {
    let parsed = parse_program(value);

    assert_eq!(parsed, Ok((platform, PathBuf::from(path))));
}

#[rstest]
#[case("target/app", "is not PLATFORM=PATH")]
#[case("darwin-aarch64=app", "unknown platform 'darwin-aarch64'")]
#[case("wasm32-wasip2=app.wasm", "unknown platform 'wasm32-wasip2'")]
#[case("arm64=app", "unknown platform 'arm64'")]
fn parse_program____not_a_native_platform____refused(#[case] value: &str, #[case] message: &str) {
    let parsed = parse_program(value);

    let error = parsed.unwrap_err();
    assert!(error.contains(message), "{error}");
}
