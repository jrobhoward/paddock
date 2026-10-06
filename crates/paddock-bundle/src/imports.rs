//! Reading a component's imports without compiling it.

use wasmparser::{ComponentTypeRef, Encoding, Parser, Payload};

use crate::error::BundleError;

/// The names of a component's top-level imports, in order
/// (`paddock:runtime/process@0.1.0`, `wasi:keyvalue/store@0.2.0-draft`).
///
/// Imports of nested core modules and components are not included; the host
/// links only the top level. Type imports are skipped too: a world that uses
/// types from an interface (`use types.{envelope}`) imports each type by its
/// plain name, bound to the interface instance that defines it, so there is
/// nothing separate to link.
///
/// # Errors
///
/// [`BundleError::InvalidComponent`] if `bytes` is not a WebAssembly
/// component.
pub fn component_imports(bytes: &[u8]) -> Result<Vec<String>, BundleError> {
    let invalid = |e: wasmparser::BinaryReaderError| BundleError::InvalidComponent(e.to_string());
    let mut imports = Vec::new();
    // Nested modules and components appear inline in `parse_all`'s output,
    // between their section payload and a matching `End`.
    let mut depth = 0usize;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(invalid)? {
            Payload::Version { encoding, .. } if depth == 0 => {
                if encoding != Encoding::Component {
                    return Err(BundleError::InvalidComponent(
                        "this is a core module, not a component".to_string(),
                    ));
                }
            }
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => break,
            },
            Payload::ComponentImportSection(reader) if depth == 0 => {
                for import in reader {
                    let import = import.map_err(invalid)?;
                    if !matches!(import.ty, ComponentTypeRef::Type(_)) {
                        imports.push(import.name.name.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    Ok(imports)
}

#[cfg(test)]
#[path = "imports_tests.rs"]
mod imports_tests;
