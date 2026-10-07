//! Upgrade errors and upgrade validation.

use std::collections::BTreeSet;

use paddock_bundle::manifest::{AppManifest, Capability, UpgradeMode};

use crate::message::ExitReason;

/// Why an upgrade did not happen, or was undone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum UpgradeError {
    /// The instance had already exited.
    #[error("instance is not running")]
    NotRunning,
    /// `save-snapshot` failed. The old version cannot continue after a trap,
    /// so the instance exited with this reason and its supervisor restarts it.
    #[error("save-snapshot failed: {0}")]
    SaveFailed(ExitReason),
    /// The new version could not be instantiated; the old version kept
    /// running.
    #[error("new version failed to start: {0}")]
    Instantiate(String),
    /// The new version's `load-snapshot` refused the snapshot or failed; the
    /// old version kept running.
    #[error("load-snapshot failed: {0}")]
    LoadFailed(String),
    /// The upgrade is not allowed as asked.
    #[error("upgrade refused: {0}")]
    Invalid(String),
    /// The new version requests capabilities the old one did not; approve
    /// them to proceed.
    #[error("new capabilities need approval: {}", keys(.0))]
    NeedsApproval(Vec<Capability>),
    /// No app by that name is running.
    #[error("no running app named {0}")]
    UnknownApp(String),
    /// Another upgrade of the app is in progress.
    #[error("an upgrade of {0} is already in progress")]
    InProgress(String),
    /// There is no previous version to roll back to.
    #[error("{0} has no previous version")]
    NoPrevious(String),
    /// The rollout stopped and every upgraded instance went back to the old
    /// version.
    #[error("rollout stopped and reverted: {0}")]
    Reverted(String),
}

fn keys(caps: &[Capability]) -> String {
    caps.iter().map(|c| c.key()).collect::<Vec<_>>().join(", ")
}

/// Check that `new` may replace `old`. Returns the capabilities `new`
/// declares that `old` did not (all approved if `approve`).
///
/// # Errors
///
/// [`UpgradeError::Invalid`] for a different app name, the same version, a
/// different WIT world, an old version outside the new manifest's
/// `upgrade.from_versions`, or `blue-green` mode;
/// [`UpgradeError::NeedsApproval`] for new capabilities without `approve`.
pub fn validate_upgrade(
    old: &AppManifest,
    new: &AppManifest,
    approve: bool,
) -> Result<Vec<Capability>, UpgradeError> {
    let invalid = |why: String| Err(UpgradeError::Invalid(why));
    if old.app.name != new.app.name {
        return invalid(format!("{} cannot replace {}", new.app.name, old.app.name));
    }
    if old.app.version == new.app.version {
        return invalid(format!(
            "{} {} is already running",
            old.app.name, old.app.version
        ));
    }
    if old.component.world != new.component.world {
        return invalid(format!(
            "world changes from {} to {}",
            old.component.world, new.component.world
        ));
    }
    let from = &new.spec.upgrade.from_versions;
    if !from.is_empty() && !from.iter().any(|req| req.matches(&old.app.version)) {
        return invalid(format!(
            "{} {} does not upgrade from {}",
            new.app.name, new.app.version, old.app.version
        ));
    }
    if new.spec.upgrade.mode == UpgradeMode::BlueGreen {
        return invalid("blue-green upgrades are not supported in Phase 1".into());
    }
    let before: BTreeSet<Capability> = old.spec.capabilities.declared().into_iter().collect();
    let added: Vec<Capability> = new
        .spec
        .capabilities
        .declared()
        .into_iter()
        .filter(|c| !before.contains(c))
        .collect();
    if !added.is_empty() && !approve {
        return Err(UpgradeError::NeedsApproval(added));
    }
    Ok(added)
}

#[cfg(test)]
#[path = "upgrade_tests.rs"]
mod upgrade_tests;
