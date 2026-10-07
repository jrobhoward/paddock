//! The `paddock` command's subcommands that do more than call the admin API.

pub(crate) mod bundle;
pub(crate) mod format;
#[cfg(feature = "node")]
pub(crate) mod node;
