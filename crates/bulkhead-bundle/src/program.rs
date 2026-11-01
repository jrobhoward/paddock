//! Native programs: the platforms a `native-process` app runs on, and the
//! check that a program is an ELF file for its platform's architecture.

use rustbridge_bundle::Platform;

use crate::BundleError;

/// The platforms a `native-process` app runs on: `bulkhead-runner`'s
/// sandbox is Linux only.
pub const NATIVE_PLATFORMS: [Platform; 2] = [Platform::LinuxX86_64, Platform::LinuxAarch64];

/// ELF `e_machine` values.
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;

/// Check that `program` is a 64-bit little-endian ELF file (an executable
/// or a shared library) for `platform`'s architecture.
///
/// # Errors
///
/// [`BundleError::NotNativePlatform`] for a platform outside
/// [`NATIVE_PLATFORMS`], and [`BundleError::WrongProgram`] for a program
/// that is not an ELF file for it.
pub fn check_program(platform: Platform, program: &[u8]) -> Result<(), BundleError> {
    let machine = match platform {
        Platform::LinuxX86_64 => EM_X86_64,
        Platform::LinuxAarch64 => EM_AARCH64,
        other => return Err(BundleError::NotNativePlatform(other.as_str().to_string())),
    };
    match elf_machine(program) {
        Ok(found) if found == machine => Ok(()),
        found => Err(BundleError::WrongProgram {
            platform: platform.as_str().to_string(),
            found: describe(found),
        }),
    }
}

/// The ELF header's machine, or what the file is instead.
fn elf_machine(program: &[u8]) -> Result<u16, &'static str> {
    let Some(header) = program.get(..20) else {
        return Err("not an ELF file");
    };
    if header[..4] != *b"\x7fELF" {
        return Err("not an ELF file");
    }
    if header[4] != 2 {
        return Err("a 32-bit ELF file");
    }
    if header[5] != 1 {
        return Err("a big-endian ELF file");
    }
    Ok(u16::from_le_bytes([header[18], header[19]]))
}

fn describe(found: Result<u16, &str>) -> String {
    match found {
        Err(what) => what.to_string(),
        Ok(EM_X86_64) => "an ELF file for x86_64".to_string(),
        Ok(EM_AARCH64) => "an ELF file for aarch64".to_string(),
        Ok(machine) => format!("an ELF file for machine {machine}"),
    }
}

#[cfg(test)]
#[path = "program_tests.rs"]
mod program_tests;
