//! `paddock cluster keygen` and `paddock cluster pubkey`: a node's key for
//! the cluster transport.
//!
//! Nodes trust each other by public key, as WireGuard peers do; there is
//! no certificate authority and nothing expires. `keygen` writes a node's
//! private key and prints its public key, which goes into every other
//! node's peers file against the node's id.

use std::path::Path;

use anyhow::Context;
use paddock_host::transport::quic::{PublicKey, generate_key, public_key_of};

/// Write a new private key to `path` (it must not exist; readable by its
/// owner only on Unix) and return its public key.
pub(crate) fn keygen(path: &Path) -> anyhow::Result<PublicKey> {
    let (private, public) = generate_key()?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("{} (it must not exist yet)", path.display()))?;
    std::io::Write::write_all(&mut file, private.as_bytes())?;
    Ok(public)
}

/// The public key of the private key at `path`.
pub(crate) fn pubkey(path: &Path) -> anyhow::Result<PublicKey> {
    Ok(public_key_of(path)?)
}

#[cfg(test)]
#[path = "cluster_tests.rs"]
mod cluster_tests;
