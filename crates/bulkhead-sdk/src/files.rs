//! Files in the directories the manifest preopens (`wasi:filesystem`):
//! the bundle's `priv/` files at `/priv`, and each volume at its guest
//! path (`/data`).
//!
//! Open them with `std::fs` through [`path`]: the program finds each
//! directory where the node says (`BULKHEAD_PREOPENS`), on the host's
//! filesystem, under the runner's Landlock rules.
//!
//! ```ignore
//! let orders = std::fs::read(bulkhead_sdk::files::path("/data/orders.json"))?;
//! ```

use std::path::PathBuf;

/// The path to open for `guest`, an absolute guest path at or under a
/// preopened directory (`/data/orders.json`): the file on the host, under
/// the preopen whose guest path is the longest prefix of `guest`. A path
/// no preopen covers, or with a `.`, `..` or empty segment, comes back
/// unchanged, and the sandbox refuses it.
#[must_use]
pub fn path(guest: &str) -> PathBuf {
    bulkhead_native::preopens::path(guest)
}
