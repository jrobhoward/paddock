#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use bulkhead_bundle::manifest::{AppInfo, AppSpec, ProgramRef, Provenance};
use serde_json::json;

use super::*;

fn manifest(name: &str, version: &str, spec: serde_json::Value) -> AppManifest {
    AppManifest {
        app: AppInfo {
            name: name.into(),
            version: version.parse().unwrap(),
            description: None,
            authors: vec![],
            license: None,
            repository: None,
        },
        program: ProgramRef {
            path: String::new(),
            checksum: String::new(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(&spec).unwrap(),
    }
}

#[test]
fn validate_upgrade____compatible____ok_with_no_new_capabilities() {
    let old = manifest(
        "c",
        "1.0.0",
        json!({ "capabilities": { "bulkhead:logging": {} } }),
    );
    let new = manifest(
        "c",
        "2.0.0",
        json!({ "capabilities": { "bulkhead:logging": {} }, "upgrade": { "from_versions": ["^1"] } }),
    );

    let added = validate_upgrade(&old, &new, false).unwrap();

    assert!(added.is_empty());
}

#[test]
fn validate_upgrade____new_capability_without_approval____needs_approval() {
    let old = manifest("c", "1.0.0", json!({}));
    let new = manifest(
        "c",
        "2.0.0",
        json!({ "capabilities": { "bulkhead:keyvalue": {} } }),
    );

    let result = validate_upgrade(&old, &new, false);

    assert_eq!(
        result,
        Err(UpgradeError::NeedsApproval(vec![Capability::KeyValue]))
    );
    assert_eq!(
        validate_upgrade(&old, &new, true),
        Ok(vec![Capability::KeyValue])
    );
}

#[test]
fn validate_upgrade____old_version_outside_from_versions____invalid() {
    let old = manifest("c", "1.0.0", json!({}));
    let new = manifest(
        "c",
        "3.0.0",
        json!({ "upgrade": { "from_versions": [">=2"] } }),
    );

    let err = validate_upgrade(&old, &new, false).unwrap_err();

    assert_eq!(
        err.to_string(),
        "upgrade refused: c 3.0.0 does not upgrade from 1.0.0"
    );
}

#[test]
fn validate_upgrade____same_version_or_other_app____invalid() {
    let old = manifest("c", "1.0.0", json!({}));

    let same = validate_upgrade(&old, &manifest("c", "1.0.0", json!({})), false);
    let other = validate_upgrade(&old, &manifest("d", "2.0.0", json!({})), false);

    assert!(matches!(same, Err(UpgradeError::Invalid(_))));
    assert!(matches!(other, Err(UpgradeError::Invalid(_))));
}

#[test]
fn validate_blue_green____mode_names_and_children____checked() {
    let blue_green = json!({ "upgrade": { "mode": "blue-green" } });
    let old = manifest("c", "1.0.0", json!({}));
    let new = manifest("c", "2.0.0", blue_green.clone());
    let snapshot = manifest("c", "2.0.0", json!({}));
    let mut named = blue_green.clone();
    named["registered_names"] = json!(["orders"]);
    let named = manifest("c", "2.0.0", named);

    let ok = validate_blue_green(&old, &new, false);
    let not_mode = validate_blue_green(&old, &snapshot, false);
    let names = validate_blue_green(&old, &named, false);

    assert_eq!(ok, Ok(Vec::new()));
    assert!(matches!(not_mode, Err(UpgradeError::Invalid(w)) if w.contains("not in blue-green")));
    assert!(matches!(names, Err(UpgradeError::Invalid(w)) if w.contains("registers names")));
}
