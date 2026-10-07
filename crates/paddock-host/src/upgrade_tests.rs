#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use paddock_bundle::manifest::{AppInfo, AppSpec, ComponentRef, Provenance};
use serde_json::json;

use super::*;

fn manifest(name: &str, version: &str, world: &str, spec: serde_json::Value) -> AppManifest {
    AppManifest {
        app: AppInfo {
            name: name.into(),
            version: version.parse().unwrap(),
            description: None,
            authors: vec![],
            license: None,
            repository: None,
        },
        component: ComponentRef {
            path: String::new(),
            checksum: String::new(),
            world: world.into(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(&spec).unwrap(),
    }
}

const W: &str = "paddock:runtime/app@0.1.0";

#[test]
fn validate_upgrade____compatible____ok_with_no_new_capabilities() {
    let old = manifest(
        "c",
        "1.0.0",
        W,
        json!({ "capabilities": { "wasi:logging": {} } }),
    );
    let new = manifest(
        "c",
        "2.0.0",
        W,
        json!({ "capabilities": { "wasi:logging": {} }, "upgrade": { "from_versions": ["^1"] } }),
    );

    let added = validate_upgrade(&old, &new, false).unwrap();

    assert!(added.is_empty());
}

#[test]
fn validate_upgrade____new_capability_without_approval____needs_approval() {
    let old = manifest("c", "1.0.0", W, json!({}));
    let new = manifest(
        "c",
        "2.0.0",
        W,
        json!({ "capabilities": { "wasi:keyvalue": {} } }),
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
    let old = manifest("c", "1.0.0", W, json!({}));
    let new = manifest(
        "c",
        "3.0.0",
        W,
        json!({ "upgrade": { "from_versions": [">=2"] } }),
    );

    let err = validate_upgrade(&old, &new, false).unwrap_err();

    assert_eq!(
        err.to_string(),
        "upgrade refused: c 3.0.0 does not upgrade from 1.0.0"
    );
}

#[test]
fn validate_upgrade____same_version_or_other_app_or_world____invalid() {
    let old = manifest("c", "1.0.0", W, json!({}));

    let same = validate_upgrade(&old, &manifest("c", "1.0.0", W, json!({})), false);
    let other = validate_upgrade(&old, &manifest("d", "2.0.0", W, json!({})), false);
    let world = validate_upgrade(
        &old,
        &manifest("c", "2.0.0", "paddock:runtime/app@0.2.0", json!({})),
        false,
    );

    assert!(matches!(same, Err(UpgradeError::Invalid(_))));
    assert!(matches!(other, Err(UpgradeError::Invalid(_))));
    assert!(matches!(world, Err(UpgradeError::Invalid(_))));
}

#[test]
fn validate_upgrade____blue_green____invalid() {
    let old = manifest("c", "1.0.0", W, json!({}));
    let new = manifest(
        "c",
        "2.0.0",
        W,
        json!({ "upgrade": { "mode": "blue-green" } }),
    );

    let result = validate_upgrade(&old, &new, false);

    assert!(matches!(result, Err(UpgradeError::Invalid(_))));
}
