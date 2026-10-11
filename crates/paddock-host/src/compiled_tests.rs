#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
// The tests check Unix modes and inodes.
#![cfg(unix)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::*;

/// The smallest component: the header alone.
const EMPTY: &[u8] = b"\0asm\x0d\x00\x01\x00";

fn engine() -> Engine {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    Engine::new(&config).unwrap()
}

/// The cache file's inode: a new one means the file was written again.
fn inode(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().ino()
}

#[test]
fn component____no_cache_file____compiles_and_keeps_it_private() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compiled/app/1.0.0.cwasm");

    let compiled = component(&engine(), EMPTY, Some(&path));

    assert!(compiled.is_ok());
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    let dir_mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!((mode, dir_mode), (0o600, 0o700));
}

#[test]
fn component____cache_file_kept____read_not_written_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compiled/app.cwasm");
    let engine = engine();
    component(&engine, EMPTY, Some(&path)).unwrap();
    let before = inode(&path);

    let again = component(&engine, EMPTY, Some(&path));

    assert!(again.is_ok());
    assert_eq!(inode(&path), before);
}

#[test]
fn component____cache_file_corrupt____compiled_again_and_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compiled/app.cwasm");
    let engine = engine();
    component(&engine, EMPTY, Some(&path)).unwrap();
    std::fs::write(&path, b"not machine code").unwrap();
    let before = inode(&path);

    let again = component(&engine, EMPTY, Some(&path));

    assert!(again.is_ok());
    assert_ne!(inode(&path), before);
    assert!(read(&engine, &path).unwrap().is_some());
}

#[test]
fn component____cache_file_group_writable____not_read_and_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compiled/app.cwasm");
    let engine = engine();
    component(&engine, EMPTY, Some(&path)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o620)).unwrap();
    let before = inode(&path);

    let refused = read(&engine, &path);
    let again = component(&engine, EMPTY, Some(&path));

    assert!(refused.is_err());
    assert!(again.is_ok());
    assert_ne!(inode(&path), before);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
