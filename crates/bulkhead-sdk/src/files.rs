//! Files in the directories the manifest preopens (`wasi:filesystem`):
//! the bundle's `priv/` files at `/priv`, and each volume at its guest
//! path (`/data`).
//!
//! Open them with `std::fs` through [`path`], so the same source works in
//! both builds: a component sees each directory at its guest path, and a
//! native program finds it where the node says (`BULKHEAD_PREOPENS`), on
//! the host's filesystem, under the runner's Landlock rules.
//!
//! ```ignore
//! let orders = std::fs::read(bulkhead_sdk::files::path("/data/orders.json"))?;
//! ```

use std::path::PathBuf;

/// The path to open for `guest`, an absolute guest path at or under a
/// preopened directory (`/data/orders.json`). In a component that is
/// `guest` itself. In a native program it is the file on the host, under
/// the preopen whose guest path is the longest prefix of `guest`; a path
/// no preopen covers, or with a `.`, `..` or empty segment, comes back
/// unchanged, and the sandbox refuses it as WASI refuses a path outside
/// every preopen.
#[must_use]
pub fn path(guest: &str) -> PathBuf {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        bulkhead_native::preopens::path(guest)
    }
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    {
        PathBuf::from(guest)
    }
}
