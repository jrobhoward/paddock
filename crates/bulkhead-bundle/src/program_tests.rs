#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;
use rstest::rstest;

/// The first 64 bytes of an ELF file: class, byte order and machine.
fn elf(class: u8, data: u8, machine: u16) -> Vec<u8> {
    let mut header = vec![0u8; 64];
    header[..4].copy_from_slice(b"\x7fELF");
    header[4] = class;
    header[5] = data;
    header[6] = 1;
    header[18..20].copy_from_slice(&machine.to_le_bytes());
    header
}

#[rstest]
#[case(Platform::LinuxX86_64, EM_X86_64)]
#[case(Platform::LinuxAarch64, EM_AARCH64)]
fn check_program____elf_for_the_platform____accepted(
    #[case] platform: Platform,
    #[case] machine: u16,
) {
    let program = elf(2, 1, machine);

    let result = check_program(platform, &program);

    assert!(result.is_ok(), "{result:?}");
}

#[rstest]
#[case(
    Platform::LinuxX86_64,
    elf(2, 1, EM_AARCH64),
    "an ELF file for aarch64"
)]
#[case(Platform::LinuxAarch64, elf(2, 1, EM_X86_64), "an ELF file for x86_64")]
#[case(Platform::LinuxAarch64, elf(2, 1, 40), "an ELF file for machine 40")]
#[case(Platform::LinuxAarch64, elf(1, 1, EM_AARCH64), "a 32-bit ELF file")]
#[case(Platform::LinuxX86_64, elf(2, 2, EM_X86_64), "a big-endian ELF file")]
#[case(Platform::LinuxX86_64, b"#!/bin/sh\necho hi\n".to_vec(), "not an ELF file")]
#[case(Platform::LinuxX86_64, b"\x7fELF".to_vec(), "not an ELF file")]
fn check_program____not_for_the_platform____refused_with_what_it_is(
    #[case] platform: Platform,
    #[case] program: Vec<u8>,
    #[case] found: &str,
) {
    let result = check_program(platform, &program);

    let Err(BundleError::WrongProgram {
        platform: p,
        found: f,
    }) = result
    else {
        panic!("expected WrongProgram, got {result:?}");
    };
    assert_eq!(p, platform.as_str());
    assert_eq!(f, found);
}

#[rstest]
#[case(Platform::DarwinAarch64)]
#[case(Platform::Wasm32Wasip2)]
fn check_program____platform_native_apps_do_not_run_on____refused(#[case] platform: Platform) {
    let program = elf(2, 1, EM_AARCH64);

    let result = check_program(platform, &program);

    assert!(
        matches!(result, Err(BundleError::NotNativePlatform(ref p)) if p == platform.as_str()),
        "{result:?}"
    );
}
