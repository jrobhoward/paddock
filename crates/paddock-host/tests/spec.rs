//! Building an instance spec from a manifest.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use paddock_bundle::manifest::{
    AppInfo, AppManifest, AppSpec, Capability, ComponentRef, Durability, LogLevel, Provenance,
};
use paddock_host::{InstanceSpec, Preopen};

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
        "registered_names": ["orders", { "name": "orders-global", "scope": "global" }],
        "limits": { "memory_max": "16MiB", "table_max": 500, "callback_deadline_ms": 250, "mailbox_max": 7, "message_max_bytes": "64KiB" },
        "lifecycle": { "init_timeout_ms": 1500, "shutdown_timeout_ms": 900, "start_args": { "mode": "fast" } }
    }));

    let spec = InstanceSpec::from_manifest(&m);

    assert_eq!(spec.app, "orders");
    assert_eq!(spec.version, "1.4.0");
    assert_eq!(spec.memory_max, 16 << 20);
    assert_eq!(spec.table_max, Some(500));
    assert_eq!(spec.callback_deadline, Duration::from_millis(250));
    assert_eq!(spec.mailbox_max, 7);
    assert_eq!(spec.message_max_bytes, 64 << 10);
    assert_eq!(spec.registered_names, vec!["orders", "orders-global"]);
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

#[test]
fn from_manifest____capabilities____granted_buckets_logging_and_config() {
    let m = manifest(serde_json::json!({
        "capabilities": {
            "wasi:logging": { "max_level": "warn", "rate_limit_per_sec": 50 },
            "wasi:keyvalue": { "buckets": { "orders": { "durability": "disk", "quota": "1MiB" } } },
            "wasi:config": {}
        },
        "config": { "defaults": { "mode": "fast" } }
    }));

    let spec = InstanceSpec::from_manifest(&m);

    let granted: Vec<Capability> = spec.granted.iter().copied().collect();
    assert_eq!(
        granted,
        vec![
            Capability::Logging,
            Capability::Config,
            Capability::KeyValue
        ]
    );
    assert_eq!(
        spec.services.keyvalue.buckets["orders"].durability,
        Durability::Disk
    );
    assert_eq!(spec.services.logging.level, LogLevel::Warn);
    assert_eq!(spec.services.logging.rate_limit, Some(50));
    assert_eq!(spec.services.config.values["mode"], "fast");
}

#[test]
fn with_granted____operator_grants_less____narrows() {
    let m =
        manifest(serde_json::json!({ "capabilities": { "wasi:logging": {}, "wasi:config": {} } }));

    let spec = InstanceSpec::from_manifest(&m).with_granted(&BTreeSet::from([Capability::Config]));

    assert_eq!(spec.granted, BTreeSet::from([Capability::Config]));
}

#[test]
fn new____defaults____nothing_granted() {
    let spec = InstanceSpec::new("x", "0.1.0");

    assert!(spec.granted.is_empty());
    assert_eq!(spec.memory_max, 64 << 20);
}

fn fs_manifest() -> AppManifest {
    manifest(serde_json::json!({
        "capabilities": { "wasi:filesystem": { "preopens": {
            "/data": { "volume": "orders-data", "mode": "rw" },
            "/ref": { "volume": "reference" }
        } } }
    }))
}

#[test]
fn resolve_preopens____volumes_known____priv_then_declared_preopens() {
    let m = fs_manifest();
    let mut spec = InstanceSpec::from_manifest(&m);

    spec.resolve_preopens(
        &m,
        |v| Some(PathBuf::from(format!("/vol/{v}"))),
        Some(Path::new("/bundles/orders/priv")),
    )
    .unwrap();

    assert_eq!(
        spec.preopens,
        vec![
            Preopen {
                host: "/bundles/orders/priv".into(),
                guest: "/priv".into(),
                writable: false
            },
            Preopen {
                host: "/vol/orders-data".into(),
                guest: "/data".into(),
                writable: true
            },
            Preopen {
                host: "/vol/reference".into(),
                guest: "/ref".into(),
                writable: false
            },
        ]
    );
}

#[test]
fn resolve_preopens____unknown_volume____error_names_it() {
    let m = fs_manifest();
    let mut spec = InstanceSpec::from_manifest(&m);

    let err = spec.resolve_preopens(&m, |_| None, None).unwrap_err();

    assert_eq!(err, "volume 'orders-data' is not configured on this node");
}

#[test]
fn resolve_preopens____manifest_claims_priv____error() {
    let m = manifest(serde_json::json!({
        "capabilities": { "wasi:filesystem": { "preopens": { "/priv": { "volume": "x" } } } }
    }));
    let mut spec = InstanceSpec::from_manifest(&m);

    let err = spec
        .resolve_preopens(&m, |v| Some(PathBuf::from(v)), None)
        .unwrap_err();

    assert!(err.contains("reserved"), "{err}");
}

#[test]
fn resolve_preopens____no_filesystem_capability____none() {
    let m = manifest(serde_json::json!({}));
    let mut spec = InstanceSpec::from_manifest(&m);

    spec.resolve_preopens(&m, |v| Some(PathBuf::from(v)), Some(Path::new("/priv-dir")))
        .unwrap();

    assert!(spec.preopens.is_empty());
}
