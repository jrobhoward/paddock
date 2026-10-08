//! The deploy store: bundles, extracted `priv/` files, and desired state.
//!
//! ```text
//! <data_dir>/
//! ├── state.json                 apps, versions, running or stopped
//! ├── bundles/<app>/<version>.rbp
//! ├── priv/<app>/<version>/...   files the app sees at /priv
//! ├── kv/                        keyvalue disk buckets
//! ├── tmp/                       uploads being verified
//! ├── node.lock                  held while a node has the directory open
//! └── audit.log                  one JSON object per line
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
        for dir in ["bundles", "priv", "kv", "tmp"] {
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

    /// Delete one version's bundle and `priv/` files.
    pub(crate) fn remove_version(&self, app: &str, version: &str) {
        let _ = std::fs::remove_file(self.bundle_path(app, version));
        let _ = std::fs::remove_dir_all(self.priv_dir(app, version));
    }

    /// Delete every version of an app.
    pub(crate) fn remove_app(&self, app: &str) {
        let _ = std::fs::remove_dir_all(self.root.join("bundles").join(app));
        let _ = std::fs::remove_dir_all(self.root.join("priv").join(app));
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

#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;
