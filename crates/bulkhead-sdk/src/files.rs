//! Files in the directories the manifest preopens (`bulkhead:files`):
//! the bundle's `priv/` files at `/priv`, and each volume at its guest
//! path (`/data`).
//!
//! Open them with [`open`], [`read`], [`write()`] and the rest, by guest
//! path. On FreeBSD the app runs in Capsicum's capability mode, which
//! refuses opening by path, and these open relative to the preopen's
//! descriptor the runner passed (ROADMAP FB2); on Linux they open the host
//! path, under the runner's Landlock rules. [`path`] gives that host path
//! for `std::fs`, which works on Linux only.
//!
//! ```ignore
//! let orders = bulkhead_sdk::files::read("/data/orders.json")?;
//! bulkhead_sdk::files::write("/data/orders.json", &orders)?;
//! ```

use std::io::{Read, Write};
use std::path::PathBuf;

pub use bulkhead_native::preopens::Mode;

/// Open the file at `guest` (`/data/orders.json`).
///
/// # Errors
///
/// No preopen covers `guest`, or the open failed (the sandbox's refusal
/// included).
pub fn open(guest: &str, mode: Mode) -> std::io::Result<std::fs::File> {
    bulkhead_native::preopens::open(guest, mode)
}

/// The whole file at `guest`.
///
/// # Errors
///
/// As [`open`], or the read failed.
pub fn read(guest: &str) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    open(guest, Mode::Read)?.read_to_end(&mut out)?;
    Ok(out)
}

/// Replace the file at `guest` with `bytes`, creating it if missing.
///
/// # Errors
///
/// As [`open`], or the write failed.
pub fn write(guest: &str, bytes: &[u8]) -> std::io::Result<()> {
    open(guest, Mode::Write)?.write_all(bytes)
}

/// Make the directory at `guest`.
///
/// # Errors
///
/// As [`open`].
pub fn create_dir(guest: &str) -> std::io::Result<()> {
    bulkhead_native::preopens::create_dir(guest)
}

/// Remove the file at `guest`.
///
/// # Errors
///
/// As [`open`].
pub fn remove_file(guest: &str) -> std::io::Result<()> {
    bulkhead_native::preopens::remove_file(guest)
}

/// The host path to open for `guest` with `std::fs` (Linux only: on
/// FreeBSD capability mode refuses it; use [`open`]), an absolute guest path at or under a
/// preopened directory (`/data/orders.json`): the file on the host, under
/// the preopen whose guest path is the longest prefix of `guest`. A path
/// no preopen covers, or with a `.`, `..` or empty segment, comes back
/// unchanged, and the sandbox refuses it.
#[must_use]
pub fn path(guest: &str) -> PathBuf {
    bulkhead_native::preopens::path(guest)
}
