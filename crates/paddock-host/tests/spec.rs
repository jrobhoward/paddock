//! Building an instance spec from a manifest.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use paddock_host::InstanceSpec;
use paddock_manifest::{AppInfo, AppManifest, AppSpec, ComponentRef, Provenance};

fn manifest(spec: serde_json::Value) -> AppManifest {
    AppManifest {
        app: AppInfo {
            name: "orders".into(),
            version: semver::Version::new(1, 4, 0),
            description: None,
            authors: vec![],
            license: None,
            repository: None,
        },
        component: ComponentRef {
            path: "lib/wasm32-wasip2/release/orders.wasm".into(),
            checksum: "sha256:00".into(),
            world: "paddock:runtime/app@0.1.0".into(),
        },
        provenance: Provenance {
            signer_key: "RWQ".into(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(&spec).unwrap(),
    }
}

#[test]
fn from_manifest____limits_and_lifecycle____carried_over() {
    let m = manifest(serde_json::json!({
        "limits": { "memory_max": "16MiB", "table_max": 500, "callback_deadline_ms": 250, "mailbox_max": 7 },
        "lifecycle": { "init_timeout_ms": 1500, "shutdown_timeout_ms": 900, "start_args": { "mode": "fast" } }
    }));

    let spec = InstanceSpec::from_manifest(&m);

    assert_eq!(spec.app, "orders");
    assert_eq!(spec.version, "1.4.0");
    assert_eq!(spec.memory_max, 16 << 20);
    assert_eq!(spec.table_max, Some(500));
    assert_eq!(spec.callback_deadline, Duration::from_millis(250));
    assert_eq!(spec.mailbox_max, 7);
    assert_eq!(spec.init_timeout, Duration::from_millis(1500));
    assert_eq!(spec.shutdown_timeout, Duration::from_millis(900));
    assert_eq!(spec.init_args, br#"{"mode":"fast"}"#);
}

#[test]
fn from_manifest____no_start_args____empty_init_args() {
    let m = manifest(serde_json::json!({}));

    let spec = InstanceSpec::from_manifest(&m);

    assert!(spec.init_args.is_empty());
}
