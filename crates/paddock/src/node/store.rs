//! The deploy store: bundles, extracted `priv/` files, and desired state.
//!
//! ```text
//! <data_dir>/
//! ├── state.json                 apps, versions, running or stopped
//! ├── bundles/<app>/<version>.rbp
//! ├── priv/<app>/<version>/...   files the app sees at /priv
//! ├── kv/                        keyvalue disk buckets
//! ├── tmp/                       uploads being verified
//! ├── failover-epoch             the highest failover epoch reached
//! ├── witness-state              a witness's epoch, the node it promoted, its record
//! ├── node.lock                  held while a node has the directory open
//! └── audit.log                  one JSON object per line
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use paddock_host::failover::WitnessState;
use serde::{Deserialize, Serialize};

use super::error::{NodeError, internal};

/// What the node should be running.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct State {
    /// Apps in deploy order.
    pub(crate) order: Vec<String>,
    pub(crate) apps: BTreeMap<String, AppState>,
}

/// One deployed app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AppState {
    pub(crate) current: String,
    #[serde(default)]
    pub(crate) previous: Option<String>,
    pub(crate) running: bool,
    /// The new version running beside `current` in a blue-green upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) new_side: Option<NewSideState>,
}

/// A blue-green upgrade's new version and its share of traffic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NewSideState {
    pub(crate) version: String,
    pub(crate) percent: u8,
}

/// The on-disk store.
#[derive(Debug)]
pub(crate) struct Store {
    root: PathBuf,
    uploads: AtomicU64,
    /// An exclusive lock on `node.lock`, released when the store drops.
    _lock: std::fs::File,
}

impl Store {
    pub(crate) fn open(root: &Path) -> Result<Self, NodeError> {
        for dir in ["bundles", "priv", "compiled", "kv", "tmp"] {
            std::fs::create_dir_all(root.join(dir))
                .map_err(|e| NodeError::Internal(format!("{}: {e}", root.join(dir).display())))?;
        }
        let path = root.join("node.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| NodeError::Internal(format!("{}: {e}", path.display())))?;
        lock.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => {
                NodeError::Conflict(format!("{} is in use by another node", root.display()))
            }
            std::fs::TryLockError::Error(e) => {
                NodeError::Internal(format!("{}: {e}", path.display()))
            }
        })?;
        Ok(Self {
            root: root.to_path_buf(),
            uploads: AtomicU64::new(0),
            _lock: lock,
        })
    }

    /// Raise and return the node's creation: 1 on the first start, one more
    /// on each start after (`paddock_host::transport::membership`).
    pub(crate) fn next_creation(&self) -> Result<u32, NodeError> {
        let path = self.root.join("creation");
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => text
                .trim()
                .parse::<u32>()
                .map_err(|e| NodeError::Internal(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(NodeError::Internal(format!("{}: {e}", path.display()))),
        };
        let next = current
            .checked_add(1)
            .filter(|&n| n <= paddock_host::pid::MAX_CREATION)
            .ok_or_else(|| {
                NodeError::Internal(format!(
                    "{}: creation would pass {}",
                    path.display(),
                    paddock_host::pid::MAX_CREATION
                ))
            })?;
        let tmp = self.root.join("tmp/creation");
        std::fs::write(&tmp, next.to_string())
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| NodeError::Internal(format!("{}: {e}", path.display())))?;
        Ok(next)
    }

    /// The highest failover epoch this node reached; 0 before any.
    pub(crate) fn load_epoch(&self) -> Result<u64, NodeError> {
        let path = self.root.join("failover-epoch");
        Ok(read_optional(&path)?
            .map(|text| text.trim().parse::<u64>())
            .transpose()
            .map_err(|e| NodeError::Internal(format!("{}: {e}", path.display())))?
            .unwrap_or(0))
    }

    /// Keeps an epoch on disk, written and synced, before this node acts
    /// at it.
    pub(crate) fn epoch_saver(&self) -> impl Fn(u64) -> Result<(), NodeError> + Send + 'static {
        let root = self.root.clone();
        move |epoch: u64| write_durable(&root, "failover-epoch", &epoch.to_string())
    }

    /// A witness's epoch, the node it promoted at it, and whether that
    /// node's partner is recorded out of date, if it ever granted one:
    /// `<epoch> <node>`, followed by ` outdated` while the record stands.
    pub(crate) fn load_witness(&self) -> Result<Option<WitnessState>, NodeError> {
        let path = self.root.join("witness-state");
        let Some(text) = read_optional(&path)? else {
            return Ok(None);
        };
        let bad = || {
            NodeError::Internal(format!(
                "{}: not \"<epoch> <node> [outdated]\"",
                path.display()
            ))
        };
        let mut parts = text.split_whitespace();
        let epoch = parts.next().and_then(|e| e.parse().ok()).ok_or_else(bad)?;
        let node = parts.next().and_then(|n| n.parse().ok()).ok_or_else(bad)?;
        let outdated = match parts.next() {
            None => false,
            Some("outdated") => true,
            Some(_) => return Err(bad()),
        };
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(Some(WitnessState::new(epoch, node, outdated)))
    }

    /// What the witness keeps: written and synced to disk before each
    /// grant or change to its record is answered.
    pub(crate) fn witness_persist(&self) -> paddock_host::failover::WitnessPersist {
        let root = self.root.clone();
        std::sync::Arc::new(move |state: WitnessState| {
            let outdated = if state.outdated { " outdated" } else { "" };
            let text = format!("{} {}{outdated}", state.epoch, state.promoted);
            write_durable(&root, "witness-state", &text).map_err(|e| e.to_string())
        })
    }

    pub(crate) fn kv_dir(&self) -> PathBuf {
        self.root.join("kv")
    }

    pub(crate) fn audit_path(&self) -> PathBuf {
        self.root.join("audit.log")
    }

    pub(crate) fn bundle_path(&self, app: &str, version: &str) -> PathBuf {
        self.root
            .join("bundles")
            .join(app)
            .join(format!("{version}.rbp"))
    }

    pub(crate) fn priv_dir(&self, app: &str, version: &str) -> PathBuf {
        self.root.join("priv").join(app).join(version)
    }

    /// Where `app`'s `version` keeps its compiled component, named after
    /// the component's checksum (`sha256:<hex>`), so a different component
    /// under the same version never reads it.
    pub(crate) fn compiled_path(&self, app: &str, version: &str, checksum: &str) -> PathBuf {
        let hex: String = checksum
            .trim_start_matches("sha256:")
            .chars()
            .filter(char::is_ascii_hexdigit)
            .take(32)
            .collect();
        self.root
            .join("compiled")
            .join(app)
            .join(version)
            .join(format!("{hex}.cwasm"))
    }

    /// Delete compiled components of apps and versions `state` no longer
    /// holds, as current or previous: those left by refused deploys and
    /// upgrades.
    pub(crate) fn prune_compiled(&self, state: &State) {
        let Ok(apps) = std::fs::read_dir(self.root.join("compiled")) else {
            return;
        };
        for app in apps.flatten() {
            let name = app.file_name().to_string_lossy().into_owned();
            let Some(kept) = state.apps.get(&name) else {
                let _ = std::fs::remove_dir_all(app.path());
                continue;
            };
            let Ok(versions) = std::fs::read_dir(app.path()) else {
                continue;
            };
            for version in versions.flatten() {
                let v = version.file_name().to_string_lossy().into_owned();
                if v != kept.current
                    && kept.previous.as_deref() != Some(v.as_str())
                    && kept.new_side.as_ref().is_none_or(|n| n.version != v)
                {
                    let _ = std::fs::remove_dir_all(version.path());
                }
            }
        }
    }

    /// Write an upload to a temporary file for verification.
    pub(crate) fn write_upload(&self, bytes: &[u8]) -> Result<PathBuf, NodeError> {
        let n = self.uploads.fetch_add(1, Ordering::Relaxed);
        let path = self
            .root
            .join("tmp")
            .join(format!("upload-{}-{n}.rbp", std::process::id()));
        std::fs::write(&path, bytes).map_err(internal)?;
        Ok(path)
    }

    /// Keep a verified upload as `app`'s `version`.
    pub(crate) fn keep_upload(
        &self,
        upload: &Path,
        app: &str,
        version: &str,
    ) -> Result<(), NodeError> {
        let target = self.bundle_path(app, version);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(internal)?;
        }
        std::fs::rename(upload, &target).map_err(internal)
    }

    /// Delete one version's bundle, `priv/` files and compiled component.
    pub(crate) fn remove_version(&self, app: &str, version: &str) {
        let _ = std::fs::remove_file(self.bundle_path(app, version));
        let _ = std::fs::remove_dir_all(self.priv_dir(app, version));
        let _ = std::fs::remove_dir_all(self.root.join("compiled").join(app).join(version));
    }

    /// Delete every version of an app.
    pub(crate) fn remove_app(&self, app: &str) {
        let _ = std::fs::remove_dir_all(self.root.join("bundles").join(app));
        let _ = std::fs::remove_dir_all(self.root.join("priv").join(app));
        let _ = std::fs::remove_dir_all(self.root.join("compiled").join(app));
    }

    pub(crate) fn load_state(&self) -> Result<State, NodeError> {
        let path = self.root.join("state.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| NodeError::Internal(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(NodeError::Internal(format!("{}: {e}", path.display()))),
        }
    }

    /// Save state atomically: write a temporary file, then rename it.
    pub(crate) fn save_state(&self, state: &State) -> Result<(), NodeError> {
        let path = self.root.join("state.json");
        let tmp = self.root.join("state.json.tmp");
        let json = serde_json::to_vec_pretty(state).map_err(internal)?;
        std::fs::write(&tmp, json).map_err(internal)?;
        std::fs::rename(&tmp, &path).map_err(internal)
    }
}

/// A file's text, or `None` if it does not exist.
fn read_optional(path: &Path) -> Result<Option<String>, NodeError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(NodeError::Internal(format!("{}: {e}", path.display()))),
    }
}

/// Replace `root/name` with `text` so that a crash leaves the old or the
/// new value, and the new one survives power loss once this returns: write
/// and sync a temporary file, rename it, sync the directory.
fn write_durable(root: &Path, name: &str, text: &str) -> Result<(), NodeError> {
    use std::io::Write;
    let path = root.join(name);
    let tmp = root.join("tmp").join(name);
    let fail = |e: std::io::Error| NodeError::Internal(format!("{}: {e}", path.display()));
    let mut file = std::fs::File::create(&tmp).map_err(fail)?;
    file.write_all(text.as_bytes()).map_err(fail)?;
    file.sync_all().map_err(fail)?;
    drop(file);
    std::fs::rename(&tmp, &path).map_err(fail)?;
    // Directories cannot be opened as files on Windows; the rename is
    // durable there once MoveFileEx returns.
    #[cfg(unix)]
    std::fs::File::open(root)
        .and_then(|d| d.sync_all())
        .map_err(fail)?;
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;
