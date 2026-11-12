//! Which user belongs to which app: the helper's own record, never the
//! node's (spike S36, finding 3). An app gets the lowest free user of the
//! range on its first spawn and keeps it from then on, across restarts of
//! the helper; a user never serves a second app. The node cannot choose
//! one: it names the app and the helper answers with its user.
//!
//! The record is `<state>/uids`, one `uid app` per line, written whole to
//! a temporary file and renamed over the old one.

use std::collections::BTreeMap;
use std::io::Write;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

/// The users given to apps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bindings {
    by_app: BTreeMap<String, u32>,
}

impl Bindings {
    /// The record in `text`.
    ///
    /// # Errors
    ///
    /// A line that is not `uid app`, an app or user given twice.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut by_app = BTreeMap::new();
        let mut used = std::collections::BTreeSet::new();
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let (uid, app) = line
                .split_once(' ')
                .ok_or_else(|| format!("line {}: not `uid app`", n + 1))?;
            let uid: u32 = uid
                .parse()
                .map_err(|_| format!("line {}: {uid} is not a user id", n + 1))?;
            if !bulkhead_priv_wire::is_app_name(app) {
                return Err(format!("line {}: {app:?} is not an app name", n + 1));
            }
            if !used.insert(uid) || by_app.insert(app.to_string(), uid).is_some() {
                return Err(format!("line {}: given twice", n + 1));
            }
        }
        Ok(Self { by_app })
    }

    /// The record's text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (app, uid) in &self.by_app {
            out.push_str(&format!("{uid} {app}\n"));
        }
        out
    }

    /// The user `app` has, if any.
    #[must_use]
    pub fn get(&self, app: &str) -> Option<u32> {
        self.by_app.get(app).copied()
    }

    /// The user for `app`: the one it has, or the lowest free one in
    /// `range`, and whether it is new (and the record must be saved). A
    /// user bound earlier but now outside `range` (the range shrank)
    /// keeps its app, which can then not be started.
    ///
    /// # Errors
    ///
    /// The app's user is outside `range`, or no user is free.
    pub fn assign(
        &mut self,
        app: &str,
        range: &RangeInclusive<u32>,
    ) -> Result<(u32, bool), String> {
        if let Some(uid) = self.get(app) {
            if !range.contains(&uid) {
                return Err(format!("{app}'s user {uid} is outside the configured uids"));
            }
            return Ok((uid, false));
        }
        let used: std::collections::BTreeSet<u32> = self.by_app.values().copied().collect();
        let uid = range
            .clone()
            .find(|u| !used.contains(u))
            .ok_or("no user left in the configured uids")?;
        self.by_app.insert(app.to_string(), uid);
        Ok((uid, true))
    }

    /// The record kept under `state`, or an empty one if there is none.
    ///
    /// # Errors
    ///
    /// It cannot be read or [`Bindings::parse`] refuses it.
    pub fn load(state: &Path) -> Result<Self, String> {
        let path = file(state);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Write the record under `state`, durably, before the user is used.
    ///
    /// # Errors
    ///
    /// It could not be written.
    pub fn save(&self, state: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;

        let path = file(state);
        let tmp = path.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(self.render().as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &path)?;
        std::fs::File::open(state)?.sync_all()
    }
}

fn file(state: &Path) -> PathBuf {
    state.join("uids")
}

#[cfg(test)]
#[path = "bindings_tests.rs"]
mod bindings_tests;
