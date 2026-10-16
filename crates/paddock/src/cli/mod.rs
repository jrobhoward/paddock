//! The `paddock` command's subcommands that do more than call the admin API.

pub(crate) mod bundle;
#[cfg(feature = "node")]
pub(crate) mod cluster;
pub(crate) mod contacts;
pub(crate) mod format;
pub(crate) mod shell;
