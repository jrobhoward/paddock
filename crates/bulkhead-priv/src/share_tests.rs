//! The test runs as an ordinary user, sharing with its own group: a
//! change of group to one's own group is allowed, so what is shared and
//! what is left alone shows in the modes.

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::*;

fn me() -> (u32, u32) {
    (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    )
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().mode() & 0o7777
}

fn set(p: &Path, m: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
}

#[test]
fn share____read_only_tree____group_reads_and_executes_where_owner_does() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().join("p");
    std::fs::create_dir_all(top.join("sub")).unwrap();
    std::fs::write(top.join("prog"), b"x").unwrap();
    std::fs::write(top.join("sub/data"), b"x").unwrap();
    set(&top, 0o700);
    set(&top.join("sub"), 0o700);
    set(&top.join("prog"), 0o500);
    set(&top.join("sub/data"), 0o600);
    let (uid, gid) = me();

    share(&top, uid, gid, false).unwrap();

    assert_eq!(mode(&top), 0o750);
    assert_eq!(mode(&top.join("sub")), 0o750);
    assert_eq!(mode(&top.join("prog")), 0o550);
    assert_eq!(mode(&top.join("sub/data")), 0o640);
}

#[test]
fn share____a_link_inside____not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().join("p");
    std::fs::create_dir(&top).unwrap();
    let outside = dir.path().join("secret");
    std::fs::write(&outside, b"x").unwrap();
    set(&outside, 0o600);
    std::os::unix::fs::symlink(&outside, top.join("link")).unwrap();
    let (uid, gid) = me();

    share(&top, uid, gid, false).unwrap();

    assert_eq!(mode(&outside), 0o600);
}

#[test]
fn share____a_hard_link_inside____left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().join("p");
    std::fs::create_dir(&top).unwrap();
    let outside = dir.path().join("secret");
    std::fs::write(&outside, b"x").unwrap();
    set(&outside, 0o600);
    std::fs::hard_link(&outside, top.join("hard")).unwrap();
    let (uid, gid) = me();

    share(&top, uid, gid, false).unwrap();

    assert_eq!(mode(&outside), 0o600);
}

#[test]
fn share____read_only_tree_others_could_read____others_read_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().join("p");
    std::fs::create_dir(&top).unwrap();
    std::fs::write(top.join("cert.pem"), b"x").unwrap();
    set(&top, 0o755);
    set(&top.join("cert.pem"), 0o644);
    let (uid, gid) = me();

    share(&top, uid, gid, false).unwrap();

    assert_eq!(mode(&top) & 0o007, 0);
    assert_eq!(mode(&top.join("cert.pem")), 0o640);
}

#[test]
fn share____writable____top_only_the_apps() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().join("v");
    std::fs::create_dir(&top).unwrap();
    std::fs::write(top.join("old"), b"x").unwrap();
    set(&top, 0o700);
    set(&top.join("old"), 0o600);
    let (uid, gid) = me();

    share(&top, uid, gid, true).unwrap();

    assert_eq!(mode(&top), 0o770);
    assert_eq!(mode(&top.join("old")), 0o600);
}

#[test]
fn share____not_the_nodes____refused() {
    let dir = tempfile::tempdir().unwrap();
    let (uid, gid) = me();

    let err = share(dir.path(), uid + 1, gid, false).unwrap_err();

    assert!(err.contains("not the node's"), "{err}");
}

#[test]
fn share____a_link_as_the_top____refused() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let (uid, gid) = me();

    assert!(share(&link, uid, gid, false).is_err());
}
