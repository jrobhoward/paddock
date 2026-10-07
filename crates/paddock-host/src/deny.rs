//! Deny stubs for imported interfaces the host does not implement.
//!
//! A component can import an interface no host here serves: a capability
//! enforced in a later phase (`wasi:http`, `paddock:sql`), or one declared
//! but ungranted that has no host at all. Rather than refuse to load, each
//! such import is linked to stubs built from the function's own type
//! (spike S4):
//!
//! - `result<_, E>` where `E` is a variant or enum with a payload-free
//!   `access-denied` case (or `permission-denied`, `access`,
//!   `HTTP-request-denied`, the names other WASI interfaces use) returns that
//!   error.
//! - `result<_, string>` returns `"access-denied"`.
//! - Anything else traps, naming the function.
//!
//! wasmtime's `define_unknown_imports_as_traps` is not used: next to the WASI
//! linker it breaks re-exported resource types (S4).

use paddock_bundle::manifest::InterfaceName;
use wasmtime::component::types::{ComponentFunc, ComponentItem};
use wasmtime::component::{Component, Linker, ResourceType, Type, Val};
use wasmtime::{Engine, Error, Result};

/// Interfaces the host links itself outside [`crate::services`]:
/// `(package, interfaces)`, `None` for the whole package.
const PROVIDED: &[(&str, Option<&[&str]>)] = &[
    ("wasi:cli", None),
    ("wasi:io", None),
    ("wasi:clocks", None),
    ("wasi:random", None),
    ("wasi:filesystem", None),
    ("wasi:sockets", None),
    ("paddock:runtime", Some(&["types", "process"])),
];

/// Error case names a stub may return, in order of preference.
const DENIED_CASES: &[&str] = &[
    "access-denied",
    "permission-denied",
    "access",
    "HTTP-request-denied",
];

/// A resource type for resources exported by a stubbed interface. No value
/// of it is ever created.
struct Denied;

fn provided(name: &str) -> bool {
    let Some(parsed) = InterfaceName::parse(name) else {
        return false;
    };
    let core = PROVIDED.iter().any(|(package, interfaces)| {
        parsed.package == *package
            && interfaces.is_none_or(|list| parsed.interface.is_some_and(|i| list.contains(&i)))
    });
    core || crate::services::INTERFACES
        .iter()
        .flat_map(|s| s.iter())
        .any(|full| {
            full.split_once('/').is_some_and(|(package, interface)| {
                parsed.package == package && parsed.interface == Some(interface)
            })
        })
}

/// Link deny stubs for every interface `component` imports that the host
/// does not provide. Returns the stubbed interface names.
pub(crate) fn link_stubs<T: 'static>(
    linker: &mut Linker<T>,
    engine: &Engine,
    component: &Component,
) -> Result<Vec<String>> {
    let mut stubbed = Vec::new();
    for (name, item) in component.component_type().imports(engine) {
        let ComponentItem::ComponentInstance(instance) = item.ty else {
            continue;
        };
        if provided(name) {
            continue;
        }
        let mut li = linker.instance(name)?;
        for (export, item) in instance.exports(engine) {
            match item.ty {
                ComponentItem::ComponentFunc(_) => {
                    let label = format!("{name}#{export}");
                    li.func_new(export, move |_store, ty, _params, results| {
                        let Some(value) = denied_value(&ty) else {
                            return Err(Error::msg(format!(
                                "access denied: {label} is not provided and has no error channel"
                            )));
                        };
                        if let Some(slot) = results.first_mut() {
                            *slot = value;
                        }
                        Ok(())
                    })?;
                }
                ComponentItem::Resource(_) => {
                    li.resource(export, ResourceType::host::<Denied>(), |_, _| Ok(()))?;
                }
                _ => {}
            }
        }
        stubbed.push(name.to_string());
    }
    Ok(stubbed)
}

/// The error a stub returns for a function of this type, if it can return
/// one.
fn denied_value(ty: &ComponentFunc) -> Option<Val> {
    let mut results = ty.results();
    let (Some(Type::Result(r)), None) = (results.next(), results.next()) else {
        return None;
    };
    let err = match r.err()? {
        Type::Variant(v) => {
            let cases: Vec<_> = v
                .cases()
                .filter(|c| c.ty.is_none())
                .map(|c| c.name.to_string())
                .collect();
            let case = DENIED_CASES
                .iter()
                .find(|n| cases.iter().any(|c| c == *n))?;
            Val::Variant((*case).to_string(), None)
        }
        Type::Enum(e) => {
            let names: Vec<String> = e.names().map(ToString::to_string).collect();
            let case = DENIED_CASES
                .iter()
                .find(|n| names.iter().any(|c| c == *n))?;
            Val::Enum((*case).to_string())
        }
        Type::String => Val::String(DENIED_CASES[0].to_string()),
        _ => return None,
    };
    Some(Val::Result(Err(Some(Box::new(err)))))
}

#[cfg(test)]
#[path = "deny_tests.rs"]
mod deny_tests;
