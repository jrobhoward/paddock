//! Byte quantities written with units (`64MiB`, `1GiB`, `500MB`).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// A number of bytes.
///
/// Parses from an integer (bytes) or a string with an optional unit: `B`,
/// binary `KiB` `MiB` `GiB` `TiB`, or decimal `KB` `MB` `GB` `TB`. Units are
/// case-sensitive and spaces between the number and the unit are allowed.
/// Serializes as a string in the largest binary unit that divides it exactly
/// (`64MiB`, `1536KiB`, `100B`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ByteSize(pub u64);

impl ByteSize {
    /// `n` kibibytes.
    #[must_use]
    pub const fn kib(n: u64) -> Self {
        Self(n * 1024)
    }

    /// `n` mebibytes.
    #[must_use]
    pub const fn mib(n: u64) -> Self {
        Self(n * 1024 * 1024)
    }

    /// `n` gibibytes.
    #[must_use]
    pub const fn gib(n: u64) -> Self {
        Self(n * 1024 * 1024 * 1024)
    }

    /// The number of bytes.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

const UNITS: &[(&str, u64)] = &[
    ("TiB", 1 << 40),
    ("GiB", 1 << 30),
    ("MiB", 1 << 20),
    ("KiB", 1 << 10),
    ("TB", 1_000_000_000_000),
    ("GB", 1_000_000_000),
    ("MB", 1_000_000),
    ("KB", 1_000),
    ("B", 1),
];

/// Why a byte size failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid byte size '{input}': {reason}")]
pub struct ParseByteSizeError {
    input: String,
    reason: &'static str,
}

impl FromStr for ByteSize {
    type Err = ParseByteSizeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |reason| ParseByteSizeError {
            input: s.to_string(),
            reason,
        };
        let trimmed = s.trim();
        let digits_end = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len());
        let (number, unit) = trimmed.split_at(digits_end);
        if number.is_empty() {
            return Err(err("expected a whole number followed by an optional unit"));
        }
        let number: u64 = number.parse().map_err(|_| err("number is too large"))?;
        let unit = unit.trim_start();
        let multiplier = if unit.is_empty() {
            1
        } else {
            UNITS
                .iter()
                .find(|(name, _)| *name == unit)
                .map(|(_, m)| *m)
                .ok_or_else(|| err("unknown unit (use B, KiB, MiB, GiB, TiB, KB, MB, GB or TB)"))?
        };
        number
            .checked_mul(multiplier)
            .map(Self)
            .ok_or_else(|| err("value is too large"))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let binary = UNITS.iter().filter(|(name, _)| name.ends_with("iB"));
        for (name, multiplier) in binary {
            if self.0 != 0 && self.0.is_multiple_of(*multiplier) {
                return write!(f, "{}{name}", self.0 / multiplier);
            }
        }
        write!(f, "{}B", self.0)
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = ByteSize;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a byte count or a string such as \"64MiB\"")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ByteSize, E> {
                Ok(ByteSize(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ByteSize, E> {
                u64::try_from(v)
                    .map(ByteSize)
                    .map_err(|_| E::custom("byte size cannot be negative"))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<ByteSize, E> {
                v.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
#[path = "bytesize_tests.rs"]
mod bytesize_tests;
