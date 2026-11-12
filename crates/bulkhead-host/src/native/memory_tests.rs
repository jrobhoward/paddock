#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

// FreeBSD reads the process group the pid leads, which a test binary may
// not (`os_tests` starts a child that does).
#[cfg(target_os = "linux")]
#[test]
fn read____this_process____its_resident_set() {
    let bytes = MemorySource::Process(std::process::id()).read();

    assert!(bytes > 0);
}

#[test]
fn read____process_gone____zero() {
    // Above either kernel's limit on pids (Linux 2^22, FreeBSD 99999).
    let bytes = MemorySource::Process(u32::MAX).read();

    assert_eq!(bytes, 0);
}

#[cfg(target_os = "linux")]
#[test]
fn read____cgroup____memory_current() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("memory.current"), "12345\n").unwrap();

    let bytes = MemorySource::Cgroup(dir.path().to_path_buf()).read();

    assert_eq!(bytes, 12345);
}

#[cfg(target_os = "linux")]
#[test]
fn read____cgroup_gone____zero() {
    let dir = tempfile::tempdir().unwrap();

    let bytes = MemorySource::Cgroup(dir.path().join("gone")).read();

    assert_eq!(bytes, 0);
}
