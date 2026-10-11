//! Failover on a node of an HA pair: `singleton` apps run only while the
//! pair's leadership lets this node run them, and `hot-standby` apps are
//! told the node's role (ROADMAP D10).
//!
//! The gate holds every singleton the node would run, in deploy order. A
//! leadership with `may_run` starts them from `init`; one without kills
//! them at once (no `terminate`: the lease may be ending).
//!
//! A singleton held but not running is checked as a start would be (its
//! dependencies, its supervision), so a node that does not run it refuses
//! what the primary would. A start that still fails when the node may run
//! singletons (a dependency stopped meanwhile) is tried again, with
//! backoff, until it succeeds or the leadership changes.
//!
//! Each start logs `singleton starting` before the app's `init` runs and
//! `singleton started` or `cannot start singleton` after; each kill logs
//! `singleton killed` once it is done. `scripts/soak.sh` reads these
//! records to check that no two nodes run a singleton at once.

use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use paddock_bundle::manifest::PlacementMode;
use paddock_host::failover::{Leadership, Standing};
use paddock_host::{AppDeployment, AppHandle, Apps, HostError, Runtime, UpgradeError};
use tokio::sync::watch;

use super::error::NodeError;

/// The first wait before trying a failed singleton start again, doubled
/// after each failure up to [`RETRY_MAX`].
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// A singleton this node would run.
#[derive(Debug)]
struct Held {
    name: String,
    current: AppDeployment,
    previous: Option<AppDeployment>,
}

/// The singletons of a node with `[failover]`.
#[derive(Debug)]
pub(super) struct Gate {
    apps: Apps,
    /// One start, stop or change of leadership at a time.
    op: tokio::sync::Mutex<()>,
    held: Mutex<Vec<Held>>,
    may_run: AtomicBool,
    closed: AtomicBool,
}

/// Whether `deployment` is a singleton.
pub(super) fn is_singleton(deployment: &AppDeployment) -> bool {
    deployment
        .manifest
        .spec
        .placement
        .as_ref()
        .is_some_and(|p| p.mode == PlacementMode::Singleton)
}

/// A standing as `failover-role` events and the API name it.
pub(super) fn role_name(standing: Standing) -> &'static str {
    match standing {
        Standing::Primary => "primary",
        Standing::Standby => "standby",
        _ => "down",
    }
}

impl Gate {
    pub(super) fn new(apps: Apps) -> Self {
        Self {
            apps,
            op: tokio::sync::Mutex::new(()),
            held: Mutex::default(),
            may_run: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Vec<Held>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The held singletons but `name`, as they would start.
    fn others(&self, name: &str) -> Vec<AppDeployment> {
        self.held()
            .iter()
            .filter(|h| h.name != name)
            .map(|h| h.current.clone())
            .collect()
    }

    /// Whether `name` is a singleton this node holds.
    pub(super) fn holds(&self, name: &str) -> bool {
        self.held().iter().any(|h| h.name == name)
    }

    /// The held singletons, in deploy order.
    pub(super) fn names(&self) -> Vec<String> {
        self.held().iter().map(|h| h.name.clone()).collect()
    }

    /// Hold a singleton, and start it if this node may run singletons now.
    pub(super) async fn hold(
        &self,
        name: &str,
        deployment: AppDeployment,
    ) -> Result<Option<AppHandle>, HostError> {
        let _op = self.op.lock().await;
        if !self.may_run.load(Ordering::Relaxed) {
            // Refuse now what a promotion could not start.
            self.apps.check_add(&deployment, &self.others(name))?;
        }
        self.held().retain(|h| h.name != name);
        self.held().push(Held {
            name: name.to_string(),
            current: deployment.clone(),
            previous: None,
        });
        if !self.may_run.load(Ordering::Relaxed) {
            return Ok(None);
        }
        tracing::info!(app = %name, "singleton starting: this node is primary");
        let added = self.apps.add(deployment).await;
        if let Err(e) = &added {
            tracing::error!(app = %name, error = %e, "cannot start singleton");
            self.held().retain(|h| h.name != name);
        }
        added.map(Some)
    }

    /// Set the version a rollback of `name` returns to.
    pub(super) fn set_previous(&self, name: &str, previous: AppDeployment) {
        if let Some(h) = self.held().iter_mut().find(|h| h.name == name) {
            h.previous = Some(previous);
        }
    }

    /// Stop holding `name`, removing it if it runs. `Ok(false)` if it is
    /// not held.
    pub(super) async fn release(&self, name: &str) -> Result<bool, HostError> {
        let _op = self.op.lock().await;
        if !self.holds(name) {
            return Ok(false);
        }
        if self.apps.app(name).is_some() {
            self.apps.remove(name).await?;
        }
        self.held().retain(|h| h.name != name);
        Ok(true)
    }

    /// After an upgrade or rollback of a running singleton: hold the
    /// versions it now runs.
    pub(super) fn sync(&self, name: &str) {
        let Some(app) = self.apps.app(name) else {
            return;
        };
        if let Some(h) = self.held().iter_mut().find(|h| h.name == name) {
            h.current = app.deployment();
            h.previous = app.previous();
        }
    }

    /// Upgrade a singleton that does not run here: the new version is
    /// what a promotion starts. Returns the version replaced and the
    /// capabilities approved.
    pub(super) fn upgrade_held(
        &self,
        name: &str,
        deployment: AppDeployment,
        approve: bool,
    ) -> Result<(String, Vec<String>), UpgradeError> {
        let current = self
            .held()
            .iter()
            .find(|h| h.name == name)
            .map(|h| h.current.manifest.clone())
            .ok_or_else(|| UpgradeError::UnknownApp(name.to_string()))?;
        let approved = paddock_host::validate_upgrade(&current, &deployment.manifest, approve)?;
        // What a promotion would start must start.
        self.apps
            .check_add(&deployment, &self.others(name))
            .map_err(|e| UpgradeError::Invalid(e.to_string()))?;
        let mut held = self.held();
        let h = held
            .iter_mut()
            .find(|h| h.name == name)
            .ok_or_else(|| UpgradeError::UnknownApp(name.to_string()))?;
        let from = h.current.manifest.app.version.to_string();
        h.previous = Some(std::mem::replace(&mut h.current, deployment));
        Ok((from, approved.iter().map(|c| c.key().to_string()).collect()))
    }

    /// Roll back a singleton that does not run here. Returns the versions
    /// from and to.
    pub(super) fn rollback_held(&self, name: &str) -> Result<(String, String), UpgradeError> {
        let mut held = self.held();
        let h = held
            .iter_mut()
            .find(|h| h.name == name)
            .ok_or_else(|| UpgradeError::UnknownApp(name.to_string()))?;
        let previous = h
            .previous
            .take()
            .ok_or_else(|| UpgradeError::NoPrevious(name.to_string()))?;
        let from = h.current.manifest.app.version.to_string();
        let to = previous.manifest.app.version.to_string();
        h.previous = Some(std::mem::replace(&mut h.current, previous));
        Ok((from, to))
    }

    /// Start or kill the held singletons as `may_run` says.
    async fn set(&self, may_run: bool) {
        let _op = self.op.lock().await;
        if self.closed.load(Ordering::Relaxed) || self.may_run.load(Ordering::Relaxed) == may_run {
            return;
        }
        self.may_run.store(may_run, Ordering::Relaxed);
        if may_run {
            self.start_held().await;
        } else {
            let names: Vec<String> = self.held().iter().map(|h| h.name.clone()).collect();
            for name in names.iter().rev() {
                if self.apps.app(name).is_none() {
                    continue;
                }
                match self.apps.kill(name).await {
                    Ok(()) => {
                        tracing::info!(app = %name, "singleton killed: this node may not run it")
                    }
                    Err(e) => tracing::error!(app = %name, error = %e, "cannot kill singleton"),
                }
            }
        }
    }

    /// Start every held singleton that does not run, in deploy order.
    /// Returns whether any could not start. Called with `op` held.
    async fn start_held(&self) -> bool {
        let held: Vec<(String, AppDeployment, Option<AppDeployment>)> = self
            .held()
            .iter()
            .map(|h| (h.name.clone(), h.current.clone(), h.previous.clone()))
            .collect();
        let mut failed = false;
        for (name, current, previous) in held {
            if self.apps.app(&name).is_some() {
                continue;
            }
            tracing::info!(app = %name, "singleton starting: this node is primary");
            match self.apps.add(current).await {
                Ok(app) => {
                    app.set_previous(previous);
                    tracing::info!(app = %name, "singleton started: this node is primary");
                }
                Err(e) => {
                    failed = true;
                    tracing::error!(app = %name, error = %e, "cannot start singleton");
                }
            }
        }
        failed
    }

    /// Whether a held singleton is missing while this node may run them:
    /// a start that failed, to try again.
    fn missing(&self) -> bool {
        self.may_run.load(Ordering::Relaxed)
            && !self.closed.load(Ordering::Relaxed)
            && self.held().iter().any(|h| self.apps.app(&h.name).is_none())
    }

    /// Try the failed starts again, if this node may still run singletons.
    async fn retry(&self) {
        let _op = self.op.lock().await;
        if self.may_run.load(Ordering::Relaxed) && !self.closed.load(Ordering::Relaxed) {
            self.start_held().await;
        }
    }

    /// Start no more singletons: the node is shutting down.
    pub(super) async fn close(&self) {
        let _op = self.op.lock().await;
        self.closed.store(true, Ordering::Relaxed);
    }
}

/// Follow the pair's leadership until `stop`: keep each new epoch on disk
/// before acting at it, tell `hot-standby` instances the role, and start or
/// kill the singletons.
pub(super) async fn follow(
    gate: std::sync::Arc<Gate>,
    rt: Runtime,
    save_epoch: impl Fn(u64) -> Result<(), NodeError> + Send + 'static,
    mut saved: u64,
    mut leadership: watch::Receiver<Leadership>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let now = leadership.borrow_and_update().clone();
        if now.epoch > saved {
            match save_epoch(now.epoch) {
                Ok(()) => saved = now.epoch,
                Err(e) => {
                    tracing::error!(error = %e, "failover epoch not kept; singletons stay stopped")
                }
            }
        }
        rt.set_failover_role(Some(role_name(now.standing)));
        gate.set(now.may_run && saved >= now.epoch).await;
        let mut wait = RETRY_MIN;
        loop {
            let retry = gate.missing();
            let woke = tokio::select! {
                changed = leadership.changed() => {
                    if changed.is_err() { return }
                    false
                }
                _ = stop.wait_for(|s| *s) => return,
                () = tokio::time::sleep(wait), if retry => true,
            };
            if !woke {
                break;
            }
            gate.retry().await;
            wait = (wait * 2).min(RETRY_MAX);
        }
    }
}
