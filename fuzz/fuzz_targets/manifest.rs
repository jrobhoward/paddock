//! Arbitrary text as an `appserver` section: parse and validate, then turn
//! an accepted section into an instance spec.

#![no_main]

use libfuzzer_sys::fuzz_target;
use paddock_bundle::manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};

fuzz_target!(|text: &str| {
    let Ok(spec) = AppSpec::from_json_str(text) else {
        return;
    };
    let manifest = AppManifest {
        app: AppInfo {
            name: "fuzz".into(),
            version: semver::Version::new(1, 0, 0),
            description: None,
            authors: Vec::new(),
            license: None,
            repository: None,
        },
        component: ComponentRef {
            path: "fuzz.wasm".into(),
            checksum: String::new(),
            world: "paddock:runtime/app@0.1.0".into(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec,
    };
    if manifest.validate().is_ok() {
        let _ = paddock_host::InstanceSpec::from_manifest(&manifest);
        let _ = manifest.spec.unenforced_fields();
    }
});
