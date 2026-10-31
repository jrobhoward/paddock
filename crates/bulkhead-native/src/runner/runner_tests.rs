#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::path::PathBuf;

use super::{Args, Program};

fn args(s: &str) -> Result<Args, String> {
    Args::parse(s.split_whitespace().map(String::from))
}

#[test]
fn parse____library_with_reads____all_kept() {
    let found =
        args("--library /b/lib.so --read /data --read /etc/x --landlock best-effort").unwrap();

    assert_eq!(found.program, Program::Library("/b/lib.so".into()));
    assert_eq!(
        found.read,
        vec![PathBuf::from("/data"), PathBuf::from("/etc/x")]
    );
    assert!(!found.landlock_required);
}

#[test]
fn parse____executable_only____landlock_required() {
    let found = args("--executable /b/app").unwrap();

    assert_eq!(found.program, Program::Executable("/b/app".into()));
    assert!(found.landlock_required);
}

#[rstest::rstest]
#[case("")]
#[case("--read /x")]
#[case("--library")]
#[case("--library /a --landlock maybe")]
#[case("--library /a --verbose")]
fn parse____bad_arguments____refused(#[case] s: &str) {
    let found = args(s);

    assert!(found.is_err(), "{found:?}");
}

#[test]
fn readable____program_directory_and_reads____included() {
    let a = args("--library /b/lib.so --read /data").unwrap();

    let found = super::sandbox::readable(&a);

    assert!(found.contains(&PathBuf::from("/b")));
    assert!(found.contains(&PathBuf::from("/data")));
    assert!(!found.iter().any(|p| p == &PathBuf::from("/b/lib.so")));
}

#[test]
fn rules____signals_and_tracing____listed() {
    let found = super::seccomp::rules(1234).unwrap();

    assert_eq!(found[&libc::SYS_kill].len(), 1);
    assert!(found[&libc::SYS_ptrace].is_empty());
    assert!(found[&libc::SYS_socket].is_empty());
}

#[test]
fn interpreter____this_test_binary____dynamic_loader() {
    let me = std::env::current_exe().unwrap();

    let found = super::sandbox::interpreter(&me).unwrap();

    let name = found.unwrap().display().to_string();
    assert!(name.contains("ld-linux"), "{name}");
}

#[rstest::rstest]
#[case(b"#!/bin/sh\n".to_vec())]
#[case(b"\x7fELF\x01\x01".to_vec())]
#[case(b"\x7fELF\x02\x01".to_vec())]
fn parse_interpreter____not_a_whole_elf64____none(#[case] bytes: Vec<u8>) {
    let found = super::sandbox::parse_interpreter(&bytes);

    assert!(found.is_none(), "{found:?}");
}
