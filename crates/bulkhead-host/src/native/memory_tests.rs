#![cfg(target_os = "linux")]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[rstest::rstest]
#[case("2345 678 90 1 0 400 0\n", Some(678))]
#[case("2345", None)]
#[case("", None)]
#[case("2345 many 90", None)]
fn parse_resident____statm_line____its_second_field(
    #[case] statm: &str,
    #[case] expected: Option<u64>,
) {
    assert_eq!(parse_resident(statm), expected);
}

#[test]
fn read____this_process____its_resident_set() {
    let bytes = MemorySource::Process(std::process::id()).read();

    assert!(bytes > 0);
}

#[test]
fn read____process_gone____zero() {
    // Above the kernel's limit on pids (2^22).
    let bytes = MemorySource::Process(u32::MAX).read();

    assert_eq!(bytes, 0);
}

#[test]
fn read____cgroup____memory_current() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("memory.current"), "12345\n").unwrap();

    let bytes = MemorySource::Cgroup(dir.path().to_path_buf()).read();

    assert_eq!(bytes, 12345);
}

#[test]
fn read____cgroup_gone____zero() {
    let dir = tempfile::tempdir().unwrap();

    let bytes = MemorySource::Cgroup(dir.path().join("gone")).read();

    assert_eq!(bytes, 0);
}
