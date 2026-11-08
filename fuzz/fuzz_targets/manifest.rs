//! Arbitrary text as an `appserver` section: parse and validate, then turn
//! an accepted section into an instance spec.

#![no_main]

use libfuzzer_sys::fuzz_target;
use bulkhead_bundle::manifest::{AppInfo, AppManifest, AppSpec, ProgramRef, Provenance};

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
        program: ProgramRef {
            path: "fuzz".into(),
            checksum: String::new(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec,
    };
    if manifest.validate().is_ok() {
        let _ = bulkhead_host::InstanceSpec::from_manifest(&manifest);
        let _ = manifest.spec.unenforced_fields();
    }
});
