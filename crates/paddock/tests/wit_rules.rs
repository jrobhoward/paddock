//! Enforces the rules stated at the top of `wit/runtime.wit`, so a new
//! interface or function cannot quietly break them.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::Path;

use wit_parser::{Function, Handle, InterfaceId, Resolve, Type, TypeDefKind, TypeId, WorldItem};

const ALWAYS_GRANTED: [&str; 2] = ["types", "process"];

fn load() -> (Resolve, wit_parser::PackageId) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../wit");
    let mut resolve = Resolve::default();
    let (pkg, _) = resolve.push_dir(&dir).expect("wit/ parses");
    (resolve, pkg)
}

fn runtime_interfaces(resolve: &Resolve, pkg: wit_parser::PackageId) -> Vec<(String, InterfaceId)> {
    resolve.packages[pkg]
        .interfaces
        .iter()
        .map(|(name, id)| (name.clone(), *id))
        .collect()
}

/// Every type reachable from `ty`, following aliases and compound types.
fn reachable(resolve: &Resolve, ty: &Type, out: &mut Vec<TypeId>) {
    let Type::Id(id) = ty else { return };
    if out.contains(id) {
        return;
    }
    out.push(*id);
    match &resolve.types[*id].kind {
        TypeDefKind::Type(t) | TypeDefKind::List(t) | TypeDefKind::Option(t) => {
            reachable(resolve, t, out)
        }
        TypeDefKind::Record(r) => r.fields.iter().for_each(|f| reachable(resolve, &f.ty, out)),
        TypeDefKind::Tuple(t) => t.types.iter().for_each(|t| reachable(resolve, t, out)),
        TypeDefKind::Variant(v) => v
            .cases
            .iter()
            .filter_map(|c| c.ty.as_ref())
            .for_each(|t| reachable(resolve, t, out)),
        TypeDefKind::Result(r) => {
            r.ok.iter()
                .chain(r.err.iter())
                .for_each(|t| reachable(resolve, t, out));
        }
        TypeDefKind::Handle(Handle::Own(t) | Handle::Borrow(t)) => out.push(*t),
        _ => {}
    }
}

fn function_types(f: &Function) -> Vec<Type> {
    f.params.iter().map(|p| p.ty).chain(f.result).collect()
}

fn all_functions(resolve: &Resolve, pkg: wit_parser::PackageId) -> Vec<(String, Function)> {
    let mut out = Vec::new();
    for (iface_name, id) in runtime_interfaces(resolve, pkg) {
        for (name, f) in &resolve.interfaces[id].functions {
            out.push((format!("{iface_name}.{name}"), f.clone()));
        }
    }
    for world in resolve.packages[pkg].worlds.values() {
        for item in resolve.worlds[*world].exports.values() {
            if let WorldItem::Function(f) = item {
                out.push((format!("export {}", f.name), f.clone()));
            }
        }
    }
    out
}

#[test]
fn package____parsed____is_paddock_runtime_0_1_0() {
    let (resolve, pkg) = load();

    let name = &resolve.packages[pkg].name;

    assert_eq!(name.to_string(), "paddock:runtime@0.1.0");
}

#[test]
fn app_world____parsed____imports_process_and_exports_every_callback() {
    let (resolve, pkg) = load();
    let world = resolve.select_world(&[pkg], Some("app")).unwrap();

    let imports: Vec<String> = resolve.worlds[world]
        .imports
        .keys()
        .map(|k| resolve.name_world_key(k))
        .collect();
    let mut exports: Vec<String> = resolve.worlds[world]
        .exports
        .keys()
        .map(|k| resolve.name_world_key(k))
        .collect();
    exports.sort();

    assert!(
        imports.contains(&"paddock:runtime/process@0.1.0".to_string()),
        "imports: {imports:?}"
    );
    assert_eq!(
        exports,
        [
            "handle-call",
            "handle-cast",
            "handle-info",
            "init",
            "load-snapshot",
            "save-snapshot",
            "terminate"
        ]
    );
}

#[test]
fn every_function____checked____is_synchronous_and_uses_no_streams_or_futures() {
    let (resolve, pkg) = load();

    let mut offenders = Vec::new();
    for (name, f) in all_functions(&resolve, pkg) {
        if f.kind.is_async() {
            offenders.push(format!("{name}: async"));
        }
        let mut seen = Vec::new();
        function_types(&f)
            .iter()
            .for_each(|t| reachable(&resolve, t, &mut seen));
        for id in seen {
            if matches!(
                resolve.types[id].kind,
                TypeDefKind::Stream(_) | TypeDefKind::Future(_)
            ) {
                offenders.push(format!("{name}: stream or future"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "WASI 0.2 sync ABI violated: {offenders:?}"
    );
}

#[test]
fn every_type____checked____comes_from_no_wasi_io_package() {
    let (resolve, pkg) = load();

    let mut offenders = Vec::new();
    for (name, f) in all_functions(&resolve, pkg) {
        let mut seen = Vec::new();
        function_types(&f)
            .iter()
            .for_each(|t| reachable(&resolve, t, &mut seen));
        for id in seen {
            let owner = match resolve.types[id].owner {
                wit_parser::TypeOwner::Interface(i) => resolve.interfaces[i].package,
                wit_parser::TypeOwner::World(w) => resolve.worlds[w].package,
                wit_parser::TypeOwner::None => None,
            };
            if let Some(p) = owner {
                let pname = &resolve.packages[p].name;
                if pname.namespace == "wasi" && pname.name == "io" {
                    offenders.push(name.clone());
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "wasi:io types exposed by: {offenders:?}"
    );
}

#[test]
fn capability_functions____checked____return_errors_with_access_denied() {
    let (resolve, pkg) = load();

    let mut offenders = Vec::new();
    for (iface_name, id) in runtime_interfaces(&resolve, pkg) {
        if ALWAYS_GRANTED.contains(&iface_name.as_str()) {
            continue;
        }
        for (name, f) in &resolve.interfaces[id].functions {
            if !returns_access_denied(&resolve, f.result.as_ref()) {
                offenders.push(format!("{iface_name}.{name}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "capability functions without an access-denied error: {offenders:?}"
    );
}

#[test]
fn call_and_register____checked____can_report_access_denied() {
    let (resolve, pkg) = load();
    let process = runtime_interfaces(&resolve, pkg)
        .into_iter()
        .find(|(n, _)| n == "process")
        .unwrap()
        .1;

    let functions = &resolve.interfaces[process].functions;

    assert!(returns_access_denied(
        &resolve,
        functions["call"].result.as_ref()
    ));
    assert!(returns_access_denied(
        &resolve,
        functions["register"].result.as_ref()
    ));
}

fn returns_access_denied(resolve: &Resolve, result: Option<&Type>) -> bool {
    let Some(Type::Id(id)) = result else {
        return false;
    };
    let TypeDefKind::Result(r) = &resolve.types[resolve_alias(resolve, *id)].kind else {
        return false;
    };
    let Some(Type::Id(err)) = r.err else {
        return false;
    };
    match &resolve.types[resolve_alias(resolve, err)].kind {
        TypeDefKind::Variant(v) => v.cases.iter().any(|c| c.name == "access-denied"),
        TypeDefKind::Enum(e) => e.cases.iter().any(|c| c.name == "access-denied"),
        _ => false,
    }
}

fn resolve_alias(resolve: &Resolve, mut id: TypeId) -> TypeId {
    while let TypeDefKind::Type(Type::Id(next)) = resolve.types[id].kind {
        id = next;
    }
    id
}

#[test]
fn paddock_packages____checked____every_function_synchronous_with_access_denied() {
    for (dir, name) in [
        ("groups", "paddock:groups@0.1.0"),
        ("watch", "paddock:watch@0.1.0"),
    ] {
        let wit = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../wit");
        let mut resolve = Resolve::default();
        resolve.push_dir(&wit).expect("wit/ parses");
        let (pkg, _) = resolve.push_dir(wit.join(dir)).expect("package parses");

        let functions: Vec<(String, Function)> = runtime_interfaces(&resolve, pkg)
            .into_iter()
            .flat_map(|(_, id)| resolve.interfaces[id].functions.clone())
            .collect();

        assert_eq!(resolve.packages[pkg].name.to_string(), name);
        assert!(!functions.is_empty());
        for (f_name, f) in &functions {
            assert!(
                matches!(f.kind, wit_parser::FunctionKind::Freestanding),
                "{name}: {f_name} is not a plain synchronous function"
            );
            assert!(
                returns_access_denied(&resolve, f.result.as_ref()),
                "{name}: {f_name} has no access-denied error"
            );
        }
    }
}
