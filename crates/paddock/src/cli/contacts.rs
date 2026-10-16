//! The shell's contacts file: how this operator reaches each node's remote
//! admin API (`[admin]`), for `connect` in `paddock shell`.
//!
//! ```toml
//! # The operator's private key, for every contact (--key overrides it).
//! key = "operator.key"
//!
//! [[node]]
//! name = "alpha"
//! id = 1                      # optional: `connect 1`, and members' CONTACT column
//! address = "10.0.0.1:7443"   # the node's [admin] address
//! node_key = "ed25519:..."    # the node's public key, pinned
//! ```
//!
//! A relative `key` is read from the contacts file's directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// The contacts, as read.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Contacts {
    /// The operator's private key for every contact.
    #[serde(default)]
    pub(crate) key: Option<PathBuf>,
    /// The nodes.
    #[serde(default, rename = "node")]
    pub(crate) nodes: Vec<Contact>,
}

/// One node.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Contact {
    /// What `connect` and the prompt call it.
    pub(crate) name: String,
    /// Its node id.
    #[serde(default)]
    pub(crate) id: Option<u32>,
    /// Its remote admin address, `host:port`.
    pub(crate) address: String,
    /// Its public key (`ed25519:...`).
    pub(crate) node_key: String,
}

/// Where the contacts file is when none is named: under
/// `$XDG_CONFIG_HOME/paddock`, or `~/.config/paddock`.
pub(crate) fn default_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("contacts.toml"))
}

/// `$XDG_CONFIG_HOME/paddock`, or `~/.config/paddock`.
pub(crate) fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("paddock"))
}

impl Contacts {
    /// Parse a contacts file's text.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let contacts: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut names = BTreeMap::new();
        let mut ids = BTreeMap::new();
        for c in &contacts.nodes {
            if c.name.is_empty() || c.name == "local" || c.name.parse::<u32>().is_ok() {
                return Err(format!(
                    "contact name {:?}: must not be empty, `local` or a number",
                    c.name
                ));
            }
            if names.insert(c.name.as_str(), ()).is_some() {
                return Err(format!("contact {} is listed twice", c.name));
            }
            if let Some(id) = c.id
                && let Some(other) = ids.insert(id, c.name.as_str())
            {
                return Err(format!("node {id} is both {other} and {}", c.name));
            }
        }
        Ok(contacts)
    }

    /// Read `path`, or the default file. A default file that does not exist
    /// is no contacts; a named one must exist.
    pub(crate) fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let (path, named) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => match default_path() {
                Some(p) => (p, false),
                None => return Ok(Self::default()),
            },
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !named => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e).with_context(|| format!("contacts {}", path.display())),
        };
        let mut contacts =
            Self::parse(&text).map_err(|e| anyhow::anyhow!("contacts {}: {e}", path.display()))?;
        if let (Some(key), Some(dir)) = (&contacts.key, path.parent())
            && key.is_relative()
        {
            contacts.key = Some(dir.join(key));
        }
        Ok(contacts)
    }

    /// The contact called `which`, or with node id `which`.
    pub(crate) fn find(&self, which: &str) -> Option<&Contact> {
        let id = which.parse::<u32>().ok();
        self.nodes
            .iter()
            .find(|c| c.name == which || (id.is_some() && c.id == id))
    }

    /// The contact whose address is `address`. Only `--remote` names an
    /// address, and it needs the `tls` feature.
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    pub(crate) fn at(&self, address: &str) -> Option<&Contact> {
        self.nodes.iter().find(|c| c.address == address)
    }

    /// Contact names by node id, for those with an id.
    pub(crate) fn names_by_id(&self) -> BTreeMap<u32, String> {
        self.nodes
            .iter()
            .filter_map(|c| Some((c.id?, c.name.clone())))
            .collect()
    }
}

#[cfg(test)]
#[path = "contacts_tests.rs"]
mod contacts_tests;
