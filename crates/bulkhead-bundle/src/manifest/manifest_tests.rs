#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;
use serde_json::json;

use super::*;

/// The `appserver` section of the example in DESIGN.md (rustbridge
/// integration → Proposed extensions).
const DESIGN_EXAMPLE: &str = r#"{
    "kind": "native-process",
    "dependencies": [{ "app": "billing", "version": "^2" }],
    "registered_names": ["orders"],
    "lifecycle": { "restart": "permanent", "shutdown_timeout_ms": 5000 },
    "instances": { "count": 2, "routing": "hash:customer_id" },
    "limits": { "memory_max": "64MiB", "callback_deadline_ms": 2000, "mailbox_max": 10000 },
    "capabilities": {
      "bulkhead:logging": { "max_level": "info" },
      "bulkhead:keyvalue": { "buckets": { "orders": { "durability": "disk", "quota": "1GiB" } } },
      "bulkhead:http-egress": { "allow": ["https://payments.internal"] },
      "bulkhead:files": { "preopens": { "/data": { "volume": "orders-data", "mode": "rw" } } }
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
        program: ProgramRef {
            path: "lib/linux-x86_64/release/orders".into(),
            checksum: "sha256:00".into(),
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
fn AppSpec____kind_wasm_component____refused_saying_so() {
    let err = AppSpec::from_json_value(&json!({ "kind": "wasm-component" }))
        .unwrap_err()
        .to_string();

    assert!(err.contains("WebAssembly components are not run"), "{err}");
}

#[test]
fn AppSpec____no_kind_or_native_section____a_native_executable() {
    let spec = AppSpec::from_json_value(&json!({})).unwrap();

    assert_eq!(spec.kind, Kind::NativeProcess);
    assert_eq!(
        spec.native.unwrap_or_default().program,
        NativeProgram::Executable
    );
}

#[test]
fn AppSpec____design_example____parses_every_section() {
    let spec = AppSpec::from_json_str(DESIGN_EXAMPLE).unwrap();

    assert_eq!(spec.kind, Kind::NativeProcess);
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
    assert_eq!(spec.limits.mailbox_max_bytes, ByteSize::mib(64));
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
    r#"{ "capabilities": { "bulkhead:telepathy": {} } }"#,
    "capabilities.bulkhead:telepathy",
    "unknown field `bulkhead:telepathy`"
)]
#[case(
    r#"{ "capabilities": { "bulkhead:keyvalue": { "buckets": { "b": { "durability": "tape" } } } } }"#,
    "capabilities.bulkhead:keyvalue.buckets.b.durability",
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

#[test]
fn AppSpec____memory_max_above_4_gib____accepted() {
    let spec: AppSpec =
        serde_json::from_value(json!({ "limits": { "memory_max": "64GiB" } })).unwrap();

    let result = spec.validate();

    assert!(result.is_ok(), "{result:?}");
}

#[rstest]
#[case(json!({ "limits": { "memory_max": 0 } }), "limits.memory_max")]
#[case(json!({ "limits": { "callback_deadline_ms": 0 } }), "limits.callback_deadline_ms")]
#[case(json!({ "limits": { "mailbox_max": 0 } }), "limits.mailbox_max")]
#[case(json!({ "limits": { "mailbox_max_bytes": "512KiB" } }), "limits.mailbox_max_bytes")]
#[case(json!({ "limits": { "message_max_bytes": "2MiB", "mailbox_max_bytes": "1MiB" } }), "limits.mailbox_max_bytes")]
#[case(json!({ "limits": { "timers_max": 0 } }), "limits.timers_max")]
#[case(json!({ "limits": { "monitors_max": 0 } }), "limits.monitors_max")]
#[case(json!({ "limits": { "links_max": 0 } }), "limits.links_max")]
#[case(json!({ "limits": { "pending_calls_max": 0 } }), "limits.pending_calls_max")]
#[case(json!({ "limits": { "groups_max": 0 } }), "limits.groups_max")]
#[case(json!({ "limits": { "watches_max": 0 } }), "limits.watches_max")]
#[case(json!({ "capabilities": { "bulkhead:keyvalue": { "shared": { "Points": "ro" } } } }), "capabilities.bulkhead:keyvalue.shared.Points")]
#[case(json!({ "capabilities": { "bulkhead:keyvalue": { "buckets": { "p": {} }, "shared": { "p": "rw" } } } }), "capabilities.bulkhead:keyvalue.shared.p")]
#[case(json!({ "capabilities": { "bulkhead:groups": { "join": ["Bad Name"] } } }), "capabilities.bulkhead:groups.join[0]")]
#[case(json!({ "capabilities": { "bulkhead:groups": { "publish": ["a", "a"] } } }), "capabilities.bulkhead:groups.publish[1]")]
#[case(json!({ "lifecycle": { "init_timeout_ms": 0 } }), "lifecycle.init_timeout_ms")]
#[case(json!({ "instances": { "count": 2, "max": 4 } }), "instances.count")]
#[case(json!({ "instances": { "min": 4, "max": 2 } }), "instances.max")]
#[case(json!({ "instances": { "count": 0 } }), "instances.count")]
#[case(json!({ "instances": { "passivate_after_ms": 0 } }), "instances.passivate_after_ms")]
#[case(json!({ "capabilities": { "bulkhead:sql": { "datasources": { "main": { "max_connections": 0 } } } } }), "capabilities.bulkhead:sql.datasources.main.max_connections")]
#[case(json!({ "registered_names": ["Orders"] }), "registered_names[0]")]
#[case(json!({ "registered_names": ["a", "a"] }), "registered_names[1]")]
#[case(json!({ "dependencies": [{ "app": "b", "version": "1" }, { "app": "b", "version": "2" }] }), "dependencies[1].app")]
#[case(json!({ "supervision": { "period_ms": 0 } }), "supervision.period_ms")]
#[case(json!({ "supervision": { "strategy": "simple_one_for_one" } }), "supervision.children")]
#[case(json!({ "supervision": { "children": [{ "id": "w", "app": "x" }, { "id": "w", "app": "y" }] } }), "supervision.children[1]")]
#[case(json!({ "supervision": { "backoff": { "initial_ms": 10, "max_ms": 5 } } }), "supervision.backoff.max_ms")]
#[case(json!({ "upgrade": { "mode": "snapshot" } }), "upgrade.snapshot_version")]
#[case(json!({ "capabilities": { "bulkhead:keyvalue": { "buckets": { "Bad Name": {} } } } }), "capabilities.bulkhead:keyvalue.buckets.Bad Name")]
#[case(json!({ "capabilities": { "bulkhead:keyvalue": { "buckets": { "b": { "quota": 0 } } } } }), "capabilities.bulkhead:keyvalue.buckets.b.quota")]
#[case(json!({ "capabilities": { "bulkhead:keyvalue": { "buckets": { "b": { "durability": "disk", "read_concurrency": true } } } } }), "capabilities.bulkhead:keyvalue.buckets.b.read_concurrency")]
#[case(json!({ "capabilities": { "bulkhead:files": { "preopens": { "data": { "volume": "v" } } } } }), "capabilities.bulkhead:files.preopens.data")]
#[case(json!({ "capabilities": { "bulkhead:files": { "preopens": { "/data/../etc": { "volume": "v" } } } } }), "capabilities.bulkhead:files.preopens./data/../etc")]
#[case(json!({ "capabilities": { "bulkhead:files": { "preopens": { "/data": { "volume": "v", "mode": "rw", "quota": "5GiB" } } } } }), "capabilities.bulkhead:files.preopens./data.quota")]
#[case(json!({ "capabilities": { "bulkhead:files": { "preopens": { "/data": { "volume": "v", "quota": "1MiB" } } } } }), "capabilities.bulkhead:files.preopens./data.quota")]
#[case(json!({ "capabilities": { "bulkhead:sockets": { "connect": [{ "cidr": "10.0.0.0/33" }] } } }), "capabilities.bulkhead:sockets.connect[0].cidr")]
#[case(json!({ "capabilities": { "bulkhead:sockets": { "max_sockets": 0 } } }), "capabilities.bulkhead:sockets.max_sockets")]
#[case(json!({ "capabilities": { "bulkhead:http-egress": { "allow": ["x"], "methods": ["get"] } } }), "capabilities.bulkhead:http-egress.methods[0]")]
#[case(json!({ "capabilities": { "bulkhead:http-routes": { "routes": [{ "path_prefix": "orders" }] } } }), "capabilities.bulkhead:http-routes.routes[0].path_prefix")]
#[case(json!({ "capabilities": { "bulkhead:http-routes": { "routes": [] } } }), "capabilities.bulkhead:http-routes.routes")]
#[case(json!({ "capabilities": { "bulkhead:http-routes": { "routes": [{ "path_prefix": "/", "hash_header": "x tenant" }] } } }), "capabilities.bulkhead:http-routes.routes[0].hash_header")]
#[case(json!({ "capabilities": { "bulkhead:http-routes": { "routes": [{ "path_prefix": "/" }], "timeout_ms": 0 } } }), "capabilities.bulkhead:http-routes.timeout_ms")]
#[case(json!({ "capabilities": { "bulkhead:logging": { "rate_limit_per_sec": 0 } } }), "capabilities.bulkhead:logging.rate_limit_per_sec")]
#[case(json!({ "api": { "messages": [{ "name": "ping", "pattern": "cast", "response_schema": "s.json" }] } }), "api.messages[0].response_schema")]
#[case(json!({ "api": { "messages": [{ "name": "a", "pattern": "call" }, { "name": "a", "pattern": "cast" }] } }), "api.messages[1].name")]
#[case(json!({ "placement": { "mode": "singleton" }, "instances": { "count": 2 } }), "placement.mode")]
#[case(json!({ "placement": { "mode": "singleton" }, "instances": { "min": 1, "max": 3 } }), "placement.mode")]
#[case(json!({ "capabilities": { "extensions": { "bulkhead:extra": {} } } }), "capabilities.extensions.bulkhead:extra")]
#[case(json!({ "capabilities": { "extensions": { "example:greeting/greet": {} } } }), "capabilities.extensions.example:greeting/greet")]
#[case(json!({ "capabilities": { "extensions": { "example:greeting": 3 } } }), "capabilities.extensions.example:greeting")]
#[case(json!({ "listeners": [{ "name": "Bad Name" }] }), "listeners[0]")]
#[case(json!({ "listeners": [{ "name": "a", "idle_timeout_ms": 0 }] }), "listeners[0].idle_timeout_ms")]
#[case(json!({ "listeners": [{ "name": "a", "max_connections": 0 }] }), "listeners[0].max_connections")]
#[case(json!({ "listeners": [{ "name": "a" }, { "name": "a" }] }), "listeners[1]")]
#[case(json!({ "health": { "interval_ms": 0 } }), "health.interval_ms")]
#[case(json!({ "messaging_acl": { "accept_from": ["web", "web"] } }), "messaging_acl.accept_from[1]")]
#[case(json!({ "messaging_acl": { "may_call": ["Bad Name"] } }), "messaging_acl.may_call[0]")]
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
            "bulkhead:logging": {},
            "bulkhead:config": {},
            "bulkhead:keyvalue": { "buckets": { "b": { "durability": "disk" } } },
            "bulkhead:files": { "preopens": { "/data": { "volume": "d", "mode": "rw" } } }
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

    assert_eq!(fields, vec!["config.secrets is not enforced",]);
}

#[test]
fn unenforced_fields____every_unenforced_field____listed() {
    let spec = AppSpec::from_json_value(&json!({
        "registered_names": [{ "name": "g", "scope": "global" }],
        "instances": { "min": 1, "max": 3 },
        "limits": { "fuel_per_message": 10, "storage_quota": "1GiB", "open_files": 4, "outbound_connections": 2 },
        "upgrade": { "mode": "blue-green" },
        "placement": { "mode": "all-nodes", "required_labels": { "zone": "a" }, "spread": "zone" },
        "health": {},
        "observability": { "metrics": true, "trace_sampling": 0.1 }
    }))
    .unwrap();

    let fields: Vec<String> = spec
        .unenforced_fields()
        .into_iter()
        .map(|f| f.path)
        .collect();

    assert_eq!(
        fields,
        vec![
            "registered_names[0].scope".to_string(),
            "instances.min".to_string(),
            "instances.max".to_string(),
            "limits.fuel_per_message".to_string(),
            "limits.storage_quota".to_string(),
            "limits.open_files".to_string(),
            "limits.outbound_connections".to_string(),
            "placement.mode".to_string(),
            "placement.required_labels".to_string(),
            "placement.spread".to_string(),
            "observability.metrics".to_string(),
            "observability.trace_sampling".to_string(),
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

#[rstest]
#[case(json!({ "native": { "program": "library" }, "instances": { "passivate_after_ms": 10 } }), "instances.passivate_after_ms")]
#[case(json!({ "native": { "program": "library" }, "upgrade": { "mode": "snapshot", "snapshot_version": 1 } }), "upgrade.mode")]
#[case(json!({ "native": { "program": "library" }, "listeners": [{ "name": "lines" }] }), "listeners")]
#[case(json!({ "kind": "native-process", "native": { "program": "executable" }, "listeners": [{ "name": "lines" }], "instances": { "passivate_after_ms": 10 } }), "instances.passivate_after_ms")]
#[case(json!({ "kind": "native-process", "native": { "program": "executable" }, "listeners": [{ "name": "lines" }], "placement": { "mode": "singleton" } }), "placement.mode")]
fn validate____native_process_outside_its_rules____refused_at_path(
    #[case] section: serde_json::Value,
    #[case] path: &str,
) {
    let found = AppSpec::from_json_value(&section);

    let Err(ManifestError::Invalid(errors)) = found else {
        panic!("{found:?}");
    };
    assert!(errors.iter().any(|e| e.path == path), "{errors:?}");
}

#[test]
fn validate____native_process_with_logging____accepted_with_program() {
    let found = AppSpec::from_json_value(&json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "capabilities": { "bulkhead:logging": {} },
        "upgrade": { "mode": "blue-green" }
    }))
    .unwrap();

    assert_eq!(found.native.unwrap().program, NativeProgram::Executable);
    assert!(found.unenforced_fields().is_empty());
}

#[test]
fn validate____native_executable_with_snapshots____accepted() {
    let found = AppSpec::from_json_value(&json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "passivate_after_ms": 10 },
        "upgrade": { "mode": "snapshot", "snapshot_version": 1 }
    }));

    assert!(found.is_ok(), "{found:?}");
}

#[test]
fn validate____native_process_with_preopens____accepted() {
    let found = AppSpec::from_json_value(&json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "capabilities": { "bulkhead:files": { "preopens": { "/data": { "volume": "orders-data", "mode": "rw" } } } },
        "upgrade": { "mode": "blue-green" }
    }));

    assert!(found.is_ok(), "{found:?}");
}

#[test]
fn validate____native_process_with_egress____accepted() {
    let found = AppSpec::from_json_value(&json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "capabilities": { "bulkhead:http-egress": { "allow": ["https://payments.internal"] } },
        "upgrade": { "mode": "blue-green" }
    }));

    assert!(found.is_ok(), "{found:?}");
}

#[test]
fn validate____native_executable_with_listeners____accepted() {
    let found = AppSpec::from_json_value(&json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "listeners": [{ "name": "lines", "max_connections": 64 }],
        "instances": { "count": 2, "routing": "hash:peer" },
        "upgrade": { "mode": "blue-green" }
    }));

    assert!(found.is_ok(), "{found:?}");
}

#[test]
fn validate____native_process_with_an_extension____accepted() {
    let found = AppSpec::from_json_value(&json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "capabilities": { "extensions": { "example:greeting": { "salutation": "Howdy" } } },
        "upgrade": { "mode": "blue-green" }
    }));

    assert!(found.is_ok(), "{found:?}");
}

#[test]
fn validate____native_process_with_sockets____accepted() {
    let found = AppSpec::from_json_value(&json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "capabilities": { "bulkhead:sockets": { "connect": [{ "cidr": "10.0.0.0/8" }] } },
        "upgrade": { "mode": "blue-green" }
    }));

    assert!(found.is_ok(), "{found:?}");
}

#[test]
fn validate____native_process_with_sql____accepted() {
    let found = AppSpec::from_json_value(&json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "capabilities": { "bulkhead:sql": { "datasources": { "main": { "max_connections": 2 } } } },
        "upgrade": { "mode": "blue-green" }
    }));

    assert!(found.is_ok(), "{found:?}");
}

#[test]
fn validate____native_process_with_keyvalue____accepted() {
    let found = AppSpec::from_json_value(&json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "capabilities": { "bulkhead:keyvalue": { "buckets": { "state": {} }, "shared": { "board": "rw" } } },
        "upgrade": { "mode": "blue-green" }
    }));

    assert!(found.is_ok(), "{found:?}");
}
