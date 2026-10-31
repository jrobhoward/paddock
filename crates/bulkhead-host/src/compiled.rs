//! Compiled components kept on disk, so loading the same component again
//! (after a restart) deserializes its machine code instead of compiling it.
//!
//! A file is used only if it and its directory belong to this process's
//! user and neither can be written by group or others: deserializing runs
//! its code as is. wasmtime refuses a
//! file from another wasmtime version or engine configuration, and that
//! component is then compiled again and the file replaced.

use std::io::Write;
use std::path::Path;

use wasmtime::Engine;
use wasmtime::component::Component;

/// `bytes` compiled for `engine`: from `cache` when it holds a usable
/// compilation, else compiled now and, with `cache`, kept there.
pub(crate) fn component(
    engine: &Engine,
    bytes: &[u8],
    cache: Option<&Path>,
) -> wasmtime::Result<Component> {
    let Some(path) = cache else {
        return Component::new(engine, bytes);
    };
    match read(engine, path) {
        Ok(Some(component)) => return Ok(component),
        Ok(None) => {}
        Err(e) => tracing::warn!(
            path = %path.display(),
            error = %e,
            "compiled component not used; compiling again"
        ),
    }
    let component = Component::new(engine, bytes)?;
    if let Err(e) = write(&component, path) {
        tracing::warn!(path = %path.display(), error = %e, "compiled component not kept");
    }
    Ok(component)
}

/// The component in `path`, `None` if there is no file.
fn read(engine: &Engine, path: &Path) -> Result<Option<Component>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    check_private(&meta, path)?;
    if let Some(dir) = path.parent() {
        let dir_meta = std::fs::metadata(dir).map_err(|e| e.to_string())?;
        check_private(&dir_meta, dir)?;
    }
    // SAFETY: `deserialize_file` runs the file's machine code as compiled
    // for this engine. The file was written by `write` below from
    // `Component::serialize`, in a directory and file no one but this
    // node's user can write (checked just above), and wasmtime checks its
    // header against this engine's version and configuration.
    let component = unsafe { Component::deserialize_file(engine, path) };
    component.map(Some).map_err(|e| e.to_string())
}

/// Refuse a file or directory another user owns, or group or others can
/// write.
fn check_private(meta: &std::fs::Metadata, path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let me = rustix::process::geteuid().as_raw();
        if meta.uid() != me {
            return Err(format!(
                "{} belongs to uid {}, not this process's {me}",
                path.display(),
                meta.uid()
            ));
        }
        if meta.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "{} can be written by group or others",
                path.display()
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (meta, path);
    Ok(())
}

/// Keep `component` in `path`: written to a temporary file beside it,
/// synced, then renamed, so a reader never sees half a file.
fn write(component: &Component, path: &Path) -> std::io::Result<()> {
    let bytes = component.serialize().map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    let tmp = path.with_extension("tmp");
    // A leftover keeps its permissions when opened; start afresh.
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
#[path = "compiled_tests.rs"]
mod compiled_tests;
