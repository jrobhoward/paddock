//! Spike S6: can Paddock read `.rbp` bundles carrying a Wasm component with
//! rustbridge-bundle 1.0.1 as a library, and what must change upstream?
//!
//! Builds three bundles by hand and opens each with `BundleLoader`:
//!   A. proposed format: bundle_version 1.2 + `wasm32-wasip2` platform + `appserver` section
//!   B. same as A but bundle_version 1.1
//!   C. today's workaround: 1.1, a native platform entry, the component as an
//!      extra file, and `appserver` as an unknown top-level field

use std::io::Write;

use rustbridge_bundle::BundleLoader;
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;

fn write_bundle(path: &std::path::Path, manifest: &serde_json::Value, files: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let opts = SimpleFileOptions::default();
    zip.start_file("manifest.json", opts).unwrap();
    zip.write_all(serde_json::to_string_pretty(manifest).unwrap().as_bytes()).unwrap();
    for (name, data) in files {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}

fn sha(data: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(data)))
}

fn appserver() -> serde_json::Value {
    serde_json::json!({
        "kind": "wasm-component",
        "registered_names": ["counter"],
        "limits": { "memory_max": "64MiB", "callback_deadline_ms": 2000 },
        "capabilities": { "wasi:logging": { "max_level": "info" } }
    })
}

fn main() {
    let wasm_path = std::env::args().nth(1).expect("usage: s6-bundle <component.wasm>");
    let wasm = std::fs::read(&wasm_path).unwrap();
    let dir = std::env::temp_dir().join(format!("paddock-s6-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let lib = "lib/wasm32-wasip2/release/counter.wasm";

    let wasm_platform = |version: &str| {
        serde_json::json!({
            "bundle_version": version,
            "plugin": { "name": "counter", "version": "1.0.0" },
            "platforms": { "wasm32-wasip2": { "variants": { "release": {
                "library": lib, "checksum": sha(&wasm),
                "component": { "world": "paddock:runtime/app@0.1.0" }
            } } } },
            "appserver": appserver()
        })
    };

    for (label, manifest) in [("A: 1.2 + wasm32-wasip2", wasm_platform("1.2")), ("B: 1.1 + wasm32-wasip2", wasm_platform("1.1"))] {
        let path = dir.join(format!("{}.rbp", &label[..1]));
        write_bundle(&path, &manifest, &[(lib, &wasm)]);
        match BundleLoader::open(&path) {
            Ok(_) => println!("{label}: opened"),
            Err(e) => println!("{label}: refused -> {e}"),
        }
    }

    // C: workaround that loads today.
    let stub = b"not a real native library";
    let manifest = serde_json::json!({
        "bundle_version": "1.1",
        "plugin": { "name": "counter", "version": "1.0.0" },
        "platforms": { "linux-x86_64": { "variants": { "release": {
            "library": "lib/linux-x86_64/release/libcounter.so", "checksum": sha(stub)
        } } } },
        "appserver": appserver(),
        "x_component": { "path": lib, "checksum": sha(&wasm) }
    });
    let path = dir.join("C.rbp");
    write_bundle(&path, &manifest, &[("lib/linux-x86_64/release/libcounter.so", stub), (lib, &wasm)]);
    let mut loader = BundleLoader::open(&path).expect("C should open");
    println!("C: 1.1 + native stub + component as extra file: opened");
    let typed = serde_json::to_value(loader.manifest()).unwrap();
    println!("   typed Manifest keeps `appserver`? {}", typed.get("appserver").is_some());
    let raw: serde_json::Value = serde_json::from_str(&loader.read_file_string("manifest.json").unwrap()).unwrap();
    println!("   raw manifest.json `appserver`: {}", raw["appserver"]);
    let component = loader.read_file(lib).unwrap();
    let expected = raw["x_component"]["checksum"].as_str().unwrap();
    println!("   component {} bytes, checksum matches: {}", component.len(), sha(&component) == expected);
    println!("   has_signatures: {}, public_key from manifest: {:?}", loader.has_signatures(), loader.public_key());
    std::fs::remove_dir_all(&dir).unwrap();
}
