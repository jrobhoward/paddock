//! Builds `.rbp` bundles for tests.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use minisign::KeyPair;
use rustbridge_bundle::{BundleBuilder, Manifest, Platform};
use serde_json::json;
use tempfile::TempDir;

/// The start of an ELF file for `platform`'s architecture.
pub fn elf(platform: Platform) -> Vec<u8> {
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

pub fn appserver() -> serde_json::Value {
    json!({
        "registered_names": ["orders"],
        "limits": { "memory_max": "32MiB" },
        "capabilities": { "wasi:keyvalue": { "buckets": { "orders": { "durability": "disk" } } } }
    })
}

/// What goes into a test bundle.
pub struct Spec {
    pub name: &'static str,
    pub version: &'static str,
    pub program: Vec<u8>,
    pub appserver: Option<serde_json::Value>,
    pub signer: Option<KeyPair>,
    /// Files added with a checksum in the manifest.
    pub files: Vec<(&'static str, Vec<u8>)>,
    /// Files added without one.
    pub raw_files: Vec<(&'static str, Vec<u8>)>,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            name: "orders",
            version: "1.4.0",
            program: elf(Platform::current().unwrap()),
            appserver: Some(appserver()),
            signer: Some(KeyPair::generate_unencrypted_keypair().unwrap()),
            files: Vec::new(),
            raw_files: Vec::new(),
        }
    }
}

impl Spec {
    pub fn public_key(&self) -> String {
        self.signer.as_ref().unwrap().pk.to_base64()
    }

    /// Write the bundle into `dir` and return its path.
    pub fn write(&self, dir: &TempDir) -> PathBuf {
        let lib = dir.path().join(self.name);
        std::fs::write(&lib, &self.program).unwrap();
        let mut builder = BundleBuilder::new(Manifest::new(self.name, self.version))
            .add_library(Platform::current().unwrap(), &lib)
            .unwrap();
        if let Some(section) = &self.appserver {
            builder = builder.with_extension("appserver", section.clone());
        }
        for (path, bytes) in &self.files {
            builder = builder.add_checksummed_bytes(path, bytes.clone());
        }
        for (path, bytes) in &self.raw_files {
            builder = builder.add_bytes(path, bytes.clone());
        }
        if let Some(keypair) = &self.signer {
            builder = builder.with_signing_key(keypair.pk.to_base64(), keypair.sk.clone());
        }
        let path = dir.path().join(format!("{}.rbp", self.name));
        builder.write(&path).unwrap();
        path
    }
}

/// Copy the bundle at `src` to `dst`, passing each file through `edit`.
pub fn rewrite(src: &Path, dst: &Path, edit: impl Fn(&str, Vec<u8>) -> Vec<u8>) {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(src).unwrap()).unwrap();
    let mut out = zip::ZipWriter::new(std::fs::File::create(dst).unwrap());
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).unwrap();
        let name = file.name().unwrap().into_owned();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        out.start_file(&name, zip::write::SimpleFileOptions::default())
            .unwrap();
        out.write_all(&edit(&name, contents)).unwrap();
    }
    out.finish().unwrap();
}

/// Copy the bundle at `src` to `dst` with one more file, `name`, added to
/// the archive as is: no checksum, and no check of its path, which
/// rustbridge's builder would refuse.
pub fn append(src: &Path, dst: &Path, name: &str, contents: &[u8]) {
    rewrite(src, dst, |_, c| c);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dst)
        .unwrap();
    let mut out = zip::ZipWriter::new_append(file).unwrap();
    out.start_file(name, zip::write::SimpleFileOptions::default())
        .unwrap();
    out.write_all(contents).unwrap();
    out.finish().unwrap();
}
