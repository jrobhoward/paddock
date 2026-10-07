//! Versioned state for snapshot upgrades.
//!
//! A snapshot is CBOR: `[format_version, state]`. Loading the current
//! format decodes the state; loading an older one hands the raw CBOR to
//! [`Snapshot::migrate`]. Derive the trait with `#[derive(Snapshot)]` and
//! `#[snapshot(version = N, migrate = path)]`.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// A decoded CBOR value, as older formats are passed to `migrate`.
pub use ciborium::Value;

/// State that can be carried across an upgrade.
pub trait Snapshot: Serialize + DeserializeOwned {
    /// The format version this type writes.
    const VERSION: u32;

    /// Read a snapshot written in an older (or newer) format.
    ///
    /// # Errors
    ///
    /// By default, always: a version this type does not know.
    fn migrate(from_version: u32, old: Value) -> Result<Self, String> {
        let _ = old;
        Err(format!(
            "cannot read snapshot format {from_version} (this version writes format {})",
            Self::VERSION
        ))
    }

    /// Encode as a snapshot.
    ///
    /// # Errors
    ///
    /// A message if the state cannot be serialized.
    fn to_snapshot(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        ciborium::into_writer(&(Self::VERSION, self), &mut out)
            .map_err(|e| format!("snapshot encode: {e}"))?;
        Ok(out)
    }

    /// Decode a snapshot, migrating an older format.
    ///
    /// # Errors
    ///
    /// A message if the bytes are not a snapshot, or `migrate` refuses.
    fn from_snapshot(bytes: &[u8]) -> Result<Self, String> {
        let (version, value): (u32, Value) =
            ciborium::from_reader(bytes).map_err(|e| format!("not a snapshot: {e}"))?;
        if version == Self::VERSION {
            value
                .deserialized()
                .map_err(|e| format!("snapshot format {version}: {e}"))
        } else {
            Self::migrate(version, value)
        }
    }
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod snapshot_tests;
