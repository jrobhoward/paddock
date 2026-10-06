#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn component_imports____interface_imports____listed_in_order() {
    let wasm = wat::parse_str(
        r#"(component
            (import "paddock:runtime/process@0.1.0" (instance))
            (import "wasi:keyvalue/store@0.2.0-draft" (instance))
        )"#,
    )
    .unwrap();

    let imports = component_imports(&wasm).unwrap();

    assert_eq!(
        imports,
        vec![
            "paddock:runtime/process@0.1.0",
            "wasi:keyvalue/store@0.2.0-draft"
        ]
    );
}

#[test]
fn component_imports____nested_component_imports____not_listed() {
    let wasm = wat::parse_str(
        r#"(component
            (import "wasi:logging/logging@0.1.0-draft" (instance))
            (component
                (import "wasi:sockets/tcp@0.2.12" (instance))
            )
            (core module (import "env" "f" (func)))
        )"#,
    )
    .unwrap();

    let imports = component_imports(&wasm).unwrap();

    assert_eq!(imports, vec!["wasi:logging/logging@0.1.0-draft"]);
}

#[test]
fn component_imports____world_level_type_imports____not_listed() {
    let wasm = wat::parse_str(
        r#"(component
            (import "paddock:runtime/types@0.1.0" (instance $types
                (type $u64 u64)
                (export "call-ref" (type (eq $u64)))
            ))
            (alias export $types "call-ref" (type $call-ref))
            (import "call-ref" (type (eq $call-ref)))
        )"#,
    )
    .unwrap();

    let imports = component_imports(&wasm).unwrap();

    assert_eq!(imports, vec!["paddock:runtime/types@0.1.0"]);
}

#[test]
fn component_imports____no_imports____returns_empty() {
    let wasm = wat::parse_str("(component)").unwrap();

    let imports = component_imports(&wasm).unwrap();

    assert!(imports.is_empty());
}

#[test]
fn component_imports____core_module____returns_invalid_component() {
    let wasm = wat::parse_str("(module)").unwrap();

    let result = component_imports(&wasm);

    assert!(matches!(result, Err(BundleError::InvalidComponent(_))));
}

#[test]
fn component_imports____garbage____returns_invalid_component() {
    let result = component_imports(b"not wasm at all");

    assert!(matches!(result, Err(BundleError::InvalidComponent(_))));
}
