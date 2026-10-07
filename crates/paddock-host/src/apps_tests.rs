#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use paddock_bundle::manifest::{AppInfo, AppSpec, ComponentRef, Provenance};

use super::*;

fn manifest(name: &str, version: &str, deps: &[(&str, &str)]) -> AppManifest {
    let deps: Vec<serde_json::Value> = deps
        .iter()
        .map(|(app, req)| serde_json::json!({ "app": app, "version": req }))
        .collect();
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
            world: String::new(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(&serde_json::json!({ "dependencies": deps })).unwrap(),
    }
}

fn names(manifests: &[AppManifest], order: &[usize]) -> Vec<String> {
    order
        .iter()
        .map(|&i| manifests[i].app.name.clone())
        .collect()
}

#[test]
fn start_order____dependencies____providers_first_otherwise_given_order() {
    let ms = [
        manifest("web", "1.0.0", &[("orders", "^1"), ("billing", "^2")]),
        manifest("orders", "1.4.0", &[("billing", "^2")]),
        manifest("billing", "2.1.0", &[]),
        manifest("audit", "0.1.0", &[]),
    ];
    let refs: Vec<&AppManifest> = ms.iter().collect();

    let order = start_order(&refs).unwrap();

    assert_eq!(
        names(&ms, &order),
        vec!["billing", "orders", "web", "audit"]
    );
}

#[test]
fn start_order____missing_dependency____error_names_it() {
    let ms = [manifest("web", "1.0.0", &[("orders", "^1")])];
    let refs: Vec<&AppManifest> = ms.iter().collect();

    let err = start_order(&refs).unwrap_err();

    assert_eq!(err, "web depends on orders, which is not deployed");
}

#[test]
fn start_order____version_mismatch____error_names_versions() {
    let ms = [
        manifest("web", "1.0.0", &[("orders", "^2")]),
        manifest("orders", "1.4.0", &[]),
    ];
    let refs: Vec<&AppManifest> = ms.iter().collect();

    let err = start_order(&refs).unwrap_err();

    assert_eq!(err, "web needs orders ^2, but 1.4.0 is deployed");
}

#[test]
fn start_order____cycle____error_lists_apps_in_it() {
    let ms = [
        manifest("a", "1.0.0", &[("b", "*")]),
        manifest("b", "1.0.0", &[("a", "*")]),
        manifest("c", "1.0.0", &[]),
    ];
    let refs: Vec<&AppManifest> = ms.iter().collect();

    let err = start_order(&refs).unwrap_err();

    assert_eq!(err, "dependency cycle among a, b");
}

#[test]
fn start_order____duplicate_names____error() {
    let ms = [manifest("a", "1.0.0", &[]), manifest("a", "2.0.0", &[])];
    let refs: Vec<&AppManifest> = ms.iter().collect();

    let result = start_order(&refs);

    assert!(result.is_err());
}
