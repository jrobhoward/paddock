#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::reason;

#[test]
fn reason____a_named_call____its_name() {
    let number = u32::try_from(libc::SYS_bind).unwrap();

    let found = reason(number + 1);

    assert_eq!(found, "syscall bind");
}

#[test]
fn reason____an_unnamed_call____its_number() {
    let found = reason(100_000);

    assert_eq!(found, "syscall 99999");
}

#[test]
fn reason____nothing_recorded____says_so() {
    let found = reason(0);

    assert!(found.starts_with("syscall refused"), "{found}");
}
