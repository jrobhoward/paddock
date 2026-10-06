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

pub const WORLD: &str = "paddock:runtime/app@0.1.0";

/// A component importing `process` and `wasi:keyvalue`.
pub fn kv_component() -> Vec<u8> {
    wat::parse_str(
        r#"(component
            (import "paddock:runtime/process@0.1.0" (instance))
            (import "wasi:keyvalue/store@0.2.0-draft" (instance))
        )"#,
    )
    .unwrap()
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
    pub component: Vec<u8>,
    pub world: &'static str,
    pub appserver: Option<serde_json::Value>,
    pub signer: Option<KeyPair>,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            name: "orders",
            version: "1.4.0",
            component: kv_component(),
            world: WORLD,
            appserver: Some(appserver()),
            signer: Some(KeyPair::generate_unencrypted_keypair().unwrap()),
        }
    }
}

impl Spec {
    pub fn public_key(&self) -> String {
        self.signer.as_ref().unwrap().pk.to_base64()
    }

    /// Write the bundle into `dir` and return its path.
    pub fn write(&self, dir: &TempDir) -> PathBuf {
        let wasm = dir.path().join(format!("{}.wasm", self.name));
        std::fs::write(&wasm, &self.component).unwrap();
        let mut builder = BundleBuilder::new(Manifest::new(self.name, self.version))
            .add_component(Platform::Wasm32Wasip2, &wasm, self.world)
            .unwrap();
        if let Some(section) = &self.appserver {
            builder = builder.with_extension("appserver", section.clone());
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
        let name = file.name().to_string();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        out.start_file(&name, zip::write::SimpleFileOptions::default())
            .unwrap();
        out.write_all(&edit(&name, contents)).unwrap();
    }
    out.finish().unwrap();
}
