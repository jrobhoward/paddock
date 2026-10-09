#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;
use serde_json::json;

use super::*;

/// The `appserver` section of the example in DESIGN.md (rustbridge
/// integration → Proposed extensions).
const DESIGN_EXAMPLE: &str = r#"{
    "kind": "wasm-component",
    "dependencies": [{ "app": "billing", "version": "^2" }],
    "registered_names": ["orders"],
    "lifecycle": { "restart": "permanent", "shutdown_timeout_ms": 5000 },
    "instances": { "count": 2, "routing": "hash:customer_id" },
    "limits": { "memory_max": "64MiB", "callback_deadline_ms": 2000, "mailbox_max": 10000 },
    "capabilities": {
      "wasi:logging": { "max_level": "info" },
      "wasi:keyvalue": { "buckets": { "orders": { "durability": "disk", "quota": "1GiB" } } },
      "wasi:http/outgoing-handler": { "allow": ["https://payments.internal"] },
      "wasi:filesystem": { "preopens": { "/data": { "volume": "orders-data", "mode": "rw", "quota": "5GiB" } } }
    },
    "messaging_acl": { "accept_from": ["web", "billing"] },
    "upgrade": { "mode": "snapshot", "snapshot_version": 3, "from_versions": [">=1.2, <1.4"] },
    "config": { "schema": "schema/config.json", "secrets": ["payments-api-key"] }
}"#;

fn invalid_paths(err: ManifestError) -> Vec<String> {
    match err {
        ManifestError::Invalid(errors) => errors.into_iter().map(|e| e.path).collect(),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

fn app_manifest(spec: AppSpec) -> AppManifest {
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
        spec,
    }
}

#[test]
fn AppSpec____design_example____parses_every_section() {
    let spec = AppSpec::from_json_str(DESIGN_EXAMPLE).unwrap();

    assert_eq!(spec.kind, Kind::WasmComponent);
    assert_eq!(spec.dependencies[0].app, "billing");
    assert!(
        spec.dependencies[0]
            .version
            .matches(&semver::Version::new(2, 3, 0))
    );
    assert_eq!(spec.registered_names[0].name, "orders");
    assert_eq!(spec.registered_names[0].scope, NameScope::Local);
    assert_eq!(spec.instances.count, Some(2));
    assert_eq!(spec.instances.routing, Routing::Hash("customer_id".into()));
    assert_eq!(spec.limits.memory_max, ByteSize::mib(64));
    assert_eq!(spec.limits.callback_deadline_ms, 2000);
    let kv = spec.capabilities.keyvalue.as_ref().unwrap();
    assert_eq!(kv.buckets["orders"].durability, Durability::Disk);
    assert_eq!(kv.buckets["orders"].quota, Some(ByteSize::gib(1)));
    let fs = spec.capabilities.filesystem.as_ref().unwrap();
    assert_eq!(fs.preopens["/data"].mode, AccessMode::Rw);
    assert_eq!(spec.upgrade.mode, UpgradeMode::Snapshot);
    assert_eq!(spec.upgrade.snapshot_version, Some(3));
    assert_eq!(spec.config.secrets, vec!["payments-api-key"]);
}

#[test]
fn AppSpec____empty_object____uses_defaults() {
    let spec = AppSpec::from_json_str("{}").unwrap();

    assert_eq!(spec, AppSpec::default());
    assert_eq!(spec.limits.memory_max, ByteSize::mib(64));
    assert_eq!(spec.limits.callback_deadline_ms, 5_000);
    assert_eq!(spec.limits.mailbox_max, 10_000);
    assert_eq!(spec.limits.message_max_bytes, ByteSize::mib(1));
    assert_eq!(spec.lifecycle.restart, RestartType::Permanent);
    assert_eq!(spec.supervision.intensity, 5);
    assert_eq!(spec.supervision.period_ms, 10_000);
    assert_eq!(spec.instances.initial(), 1);
    assert_eq!(spec.upgrade.mode, UpgradeMode::Restart);
    assert!(spec.capabilities.declared().is_empty());
}

#[rstest]
#[case("any", PlacementMode::Any)]
#[case("singleton", PlacementMode::Singleton)]
#[case("all-nodes", PlacementMode::AllNodes)]
#[case("hot-standby", PlacementMode::HotStandby)]
fn Placement____each_mode____parses(#[case] name: &str, #[case] mode: PlacementMode) {
    let spec = AppSpec::from_json_value(&json!({ "placement": { "mode": name } })).unwrap();

    assert_eq!(spec.placement.map(|p| p.mode), Some(mode));
}

#[test]
fn Placement____mode_omitted____any() {
    let spec = AppSpec::from_json_value(&json!({ "placement": { "spread": "zone" } })).unwrap();

    assert_eq!(spec.placement.map(|p| p.mode), Some(PlacementMode::Any));
}

#[test]
fn AppSpec____json_round_trip____is_lossless() {
    let spec = AppSpec::from_json_str(DESIGN_EXAMPLE).unwrap();

    let json = serde_json::to_string(&spec).unwrap();
    let reparsed = AppSpec::from_json_str(&json).unwrap();

    assert_eq!(reparsed, spec);
}

#[rstest]
#[case(r#"{ "limts": {} }"#, "limts", "unknown field `limts`")]
#[case(
    r#"{ "limits": { "memory": "1MiB" } }"#,
    "limits.memory",
    "unknown field `memory`"
)]
#[case(
    r#"{ "limits": { "memory_max": "64 parsecs" } }"#,
    "limits.memory_max",
    "unknown unit"
)]
#[case(
    r#"{ "capabilities": { "wasi:telepathy": {} } }"#,
    "capabilities.wasi:telepathy",
    "unknown field `wasi:telepathy`"
)]
#[case(
    r#"{ "capabilities": { "wasi:keyvalue": { "buckets": { "b": { "durability": "tape" } } } } }"#,
    "capabilities.wasi:keyvalue.buckets.b.durability",
    "unknown variant `tape`"
)]
#[case(
    r#"{ "instances": { "routing": "random" } }"#,
    "instances.routing",
    "invalid routing"
)]
#[case(
    r#"{ "placement": { "singleton": true } }"#,
    "placement.singleton",
    "unknown field `singleton`"
)]
#[case(
    r#"{ "placement": { "mode": "everywhere" } }"#,
    "placement.mode",
    "unknown variant `everywhere`"
)]
#[case(
    r#"{ "dependencies": [{ "app": "b", "version": "two" }] }"#,
    "dependencies[0].version",
    ""
)]
fn AppSpec____malformed____parse_error_names_path(
    #[case] json: &str,
    #[case] path: &str,
    #[case] message: &str,
) {
    let err = AppSpec::from_json_str(json).unwrap_err();

    let ManifestError::Parse {
        path: got,
        message: msg,
    } = err
    else {
        panic!("expected Parse, got {err:?}");
    };
    assert_eq!(got, path);
    assert!(msg.contains(message), "{msg}");
}

#[rstest]
#[case(json!({ "limits": { "memory_max": 0 } }), "limits.memory_max")]
#[case(json!({ "limits": { "memory_max": "5GiB" } }), "limits.memory_max")]
#[case(json!({ "limits": { "callback_deadline_ms": 0 } }), "limits.callback_deadline_ms")]
#[case(json!({ "limits": { "mailbox_max": 0 } }), "limits.mailbox_max")]
#[case(json!({ "limits": { "timers_max": 0 } }), "limits.timers_max")]
#[case(json!({ "limits": { "monitors_max": 0 } }), "limits.monitors_max")]
#[case(json!({ "limits": { "links_max": 0 } }), "limits.links_max")]
#[case(json!({ "limits": { "pending_calls_max": 0 } }), "limits.pending_calls_max")]
#[case(json!({ "limits": { "groups_max": 0 } }), "limits.groups_max")]
#[case(json!({ "capabilities": { "wasi:keyvalue": { "shared": { "Points": "ro" } } } }), "capabilities.wasi:keyvalue.shared.Points")]
#[case(json!({ "capabilities": { "wasi:keyvalue": { "buckets": { "p": {} }, "shared": { "p": "rw" } } } }), "capabilities.wasi:keyvalue.shared.p")]
#[case(json!({ "capabilities": { "paddock:groups": { "join": ["Bad Name"] } } }), "capabilities.paddock:groups.join[0]")]
#[case(json!({ "capabilities": { "paddock:groups": { "publish": ["a", "a"] } } }), "capabilities.paddock:groups.publish[1]")]
#[case(json!({ "lifecycle": { "init_timeout_ms": 0 } }), "lifecycle.init_timeout_ms")]
#[case(json!({ "instances": { "count": 2, "max": 4 } }), "instances.count")]
#[case(json!({ "instances": { "min": 4, "max": 2 } }), "instances.max")]
#[case(json!({ "instances": { "count": 0 } }), "instances.count")]
#[case(json!({ "registered_names": ["Orders"] }), "registered_names[0]")]
#[case(json!({ "registered_names": ["a", "a"] }), "registered_names[1]")]
#[case(json!({ "dependencies": [{ "app": "b", "version": "1" }, { "app": "b", "version": "2" }] }), "dependencies[1].app")]
#[case(json!({ "supervision": { "period_ms": 0 } }), "supervision.period_ms")]
#[case(json!({ "supervision": { "strategy": "simple_one_for_one" } }), "supervision.children")]
#[case(json!({ "supervision": { "children": [{ "id": "w", "app": "x" }, { "id": "w", "app": "y" }] } }), "supervision.children[1]")]
#[case(json!({ "supervision": { "backoff": { "initial_ms": 10, "max_ms": 5 } } }), "supervision.backoff.max_ms")]
#[case(json!({ "upgrade": { "mode": "snapshot" } }), "upgrade.snapshot_version")]
#[case(json!({ "capabilities": { "wasi:keyvalue": { "buckets": { "Bad Name": {} } } } }), "capabilities.wasi:keyvalue.buckets.Bad Name")]
#[case(json!({ "capabilities": { "wasi:keyvalue": { "buckets": { "b": { "quota": 0 } } } } }), "capabilities.wasi:keyvalue.buckets.b.quota")]
#[case(json!({ "capabilities": { "wasi:filesystem": { "preopens": { "data": { "volume": "v" } } } } }), "capabilities.wasi:filesystem.preopens.data")]
#[case(json!({ "capabilities": { "wasi:filesystem": { "preopens": { "/data/../etc": { "volume": "v" } } } } }), "capabilities.wasi:filesystem.preopens./data/../etc")]
#[case(json!({ "capabilities": { "wasi:sockets": { "connect": [{ "cidr": "10.0.0.0/33" }] } } }), "capabilities.wasi:sockets.connect[0].cidr")]
#[case(json!({ "capabilities": { "wasi:http/outgoing-handler": { "allow": ["x"], "methods": ["get"] } } }), "capabilities.wasi:http/outgoing-handler.methods[0]")]
#[case(json!({ "capabilities": { "wasi:http/incoming-handler": { "routes": [{ "path_prefix": "orders" }] } } }), "capabilities.wasi:http/incoming-handler.routes[0].path_prefix")]
#[case(json!({ "capabilities": { "wasi:logging": { "rate_limit_per_sec": 0 } } }), "capabilities.wasi:logging.rate_limit_per_sec")]
#[case(json!({ "api": { "messages": [{ "name": "ping", "pattern": "cast", "response_schema": "s.json" }] } }), "api.messages[0].response_schema")]
#[case(json!({ "api": { "messages": [{ "name": "a", "pattern": "call" }, { "name": "a", "pattern": "cast" }] } }), "api.messages[1].name")]
#[case(json!({ "placement": { "mode": "singleton" }, "instances": { "count": 2 } }), "placement.mode")]
#[case(json!({ "placement": { "mode": "singleton" }, "instances": { "min": 1, "max": 3 } }), "placement.mode")]
#[case(json!({ "capabilities": { "extensions": { "wasi:extra": {} } } }), "capabilities.extensions.wasi:extra")]
#[case(json!({ "capabilities": { "extensions": { "example:greeting/greet": {} } } }), "capabilities.extensions.example:greeting/greet")]
#[case(json!({ "capabilities": { "extensions": { "example:greeting": 3 } } }), "capabilities.extensions.example:greeting")]
#[case(json!({ "listeners": [{ "name": "Bad Name" }] }), "listeners[0]")]
#[case(json!({ "listeners": [{ "name": "a", "idle_timeout_ms": 0 }] }), "listeners[0].idle_timeout_ms")]
#[case(json!({ "listeners": [{ "name": "a", "max_connections": 0 }] }), "listeners[0].max_connections")]
#[case(json!({ "listeners": [{ "name": "a" }, { "name": "a" }] }), "listeners[1]")]
#[case(json!({ "health": { "interval_ms": 0 } }), "health.interval_ms")]
#[case(json!({ "observability": { "trace_sampling": 1.5 } }), "observability.trace_sampling")]
fn AppSpec____rule_broken____invalid_at_path(#[case] value: serde_json::Value, #[case] path: &str) {
    let err = AppSpec::from_json_value(&value).unwrap_err();

    let paths = invalid_paths(err);

    assert_eq!(paths, vec![path.to_string()]);
}

#[test]
fn AppSpec____several_rules_broken____reports_all() {
    let value =
        json!({ "limits": { "memory_max": 0, "mailbox_max": 0 }, "registered_names": ["X"] });

    let paths = invalid_paths(AppSpec::from_json_value(&value).unwrap_err());

    assert_eq!(
        paths,
        vec![
            "registered_names[0]",
            "limits.memory_max",
            "limits.mailbox_max"
        ]
    );
}

#[test]
fn RegisteredName____global_scope____round_trips_as_object() {
    let spec = AppSpec::from_json_value(&json!({
        "registered_names": ["local-one", { "name": "global-one", "scope": "global" }]
    }))
    .unwrap();

    let json = serde_json::to_value(&spec.registered_names).unwrap();

    assert_eq!(spec.registered_names[1].scope, NameScope::Global);
    assert_eq!(
        json,
        json!(["local-one", { "name": "global-one", "scope": "global" }])
    );
}

#[test]
fn unenforced_fields____phase1_only_manifest____returns_empty() {
    let spec = AppSpec::from_json_value(&json!({
        "registered_names": ["orders"],
        "instances": { "count": 2 },
        "limits": { "memory_max": "32MiB", "table_max": 1000 },
        "capabilities": {
            "wasi:logging": {},
            "wasi:config": {},
            "wasi:keyvalue": { "buckets": { "b": { "durability": "disk" } } },
            "wasi:filesystem": { "preopens": { "/data": { "volume": "d", "mode": "rw" } } }
        },
        "upgrade": { "mode": "snapshot", "snapshot_version": 1 }
    }))
    .unwrap();

    let fields = spec.unenforced_fields();

    assert!(fields.is_empty(), "{fields:?}");
}

#[test]
fn unenforced_fields____design_example____lists_later_phase_fields() {
    let spec = AppSpec::from_json_str(DESIGN_EXAMPLE).unwrap();

    let fields: Vec<String> = spec
        .unenforced_fields()
        .iter()
        .map(ToString::to_string)
        .collect();

    assert_eq!(
        fields,
        vec![
            "capabilities.wasi:http/outgoing-handler is not enforced until Phase 2",
            "capabilities.wasi:filesystem.preopens./data.quota is not enforced until a later phase",
            "messaging_acl is not enforced until Phase 2",
            "config.secrets is not enforced until Phase 2",
        ]
    );
}

#[test]
fn unenforced_fields____every_later_section____listed_with_phase() {
    let spec = AppSpec::from_json_value(&json!({
        "kind": "native-process",
        "registered_names": [{ "name": "g", "scope": "global" }],
        "instances": { "min": 1, "max": 3 },
        "limits": { "fuel_per_message": 10, "storage_quota": "1GiB", "open_files": 4, "outbound_connections": 2 },
        "capabilities": {
            "wasi:sockets": {},
            "paddock:sql": { "datasource": "main", "max_connections": 4 },
            "paddock:admin": {},
            "wasi:keyvalue": { "buckets": { "r": { "durability": "replicated" } } }
        },
        "upgrade": { "mode": "blue-green" },
        "placement": { "mode": "hot-standby" },
        "health": {},
        "observability": { "metrics": true, "trace_sampling": 0.1 }
    }))
    .unwrap();

    let fields: Vec<(String, Phase)> = spec
        .unenforced_fields()
        .into_iter()
        .map(|f| (f.path, f.phase))
        .collect();

    assert_eq!(
        fields,
        vec![
            ("kind".into(), Phase::Later),
            ("registered_names[0].scope".into(), Phase::Phase2),
            ("instances.min".into(), Phase::Later),
            ("instances.max".into(), Phase::Later),
            ("limits.fuel_per_message".into(), Phase::Later),
            ("limits.storage_quota".into(), Phase::Later),
            ("limits.open_files".into(), Phase::Later),
            ("limits.outbound_connections".into(), Phase::Later),
            ("capabilities.wasi:sockets".into(), Phase::Phase3),
            ("capabilities.paddock:sql".into(), Phase::Phase3),
            ("capabilities.paddock:admin".into(), Phase::Later),
            (
                "capabilities.wasi:keyvalue.buckets.r.durability".into(),
                Phase::Phase3
            ),
            ("upgrade.mode".into(), Phase::Later),
            ("placement".into(), Phase::Phase3),
            ("health".into(), Phase::Phase2),
            ("observability.metrics".into(), Phase::Phase2),
            ("observability.trace_sampling".into(), Phase::Phase2),
        ]
    );
}

#[test]
fn AppManifest____valid____passes() {
    let manifest = app_manifest(AppSpec::from_json_str(DESIGN_EXAMPLE).unwrap());

    let result = manifest.validate();

    assert!(result.is_ok());
}

#[test]
fn AppManifest____depends_on_itself____invalid_at_appserver_path() {
    let spec = AppSpec::from_json_value(&json!({
        "dependencies": [{ "app": "orders", "version": "*" }]
    }))
    .unwrap();

    let paths = invalid_paths(app_manifest(spec).validate().unwrap_err());

    assert_eq!(paths, vec!["appserver.dependencies[0].app"]);
}

#[test]
fn AppManifest____invalid_app_name____invalid_at_plugin_name() {
    let mut manifest = app_manifest(AppSpec::default());
    manifest.app.name = "My App".into();

    let paths = invalid_paths(manifest.validate().unwrap_err());

    assert_eq!(paths, vec!["plugin.name"]);
}
