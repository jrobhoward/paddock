//! Failover on a node of an HA pair: `singleton` apps run only while the
//! pair's leadership lets this node run them, and `hot-standby` apps are
//! told the node's role (ROADMAP D10).
//!
//! The gate holds every singleton the node would run, in deploy order. A
//! leadership with `may_run` starts them from `init`; one without kills
//! them at once (no `terminate`: the lease may be ending).
//!
//! A singleton with a replicated bucket (its own `replicated` bucket, or a
//! `cluster` shared bucket) also waits until replication reports this
//! node's copy holding every committed write at the current epoch (R5), so
//! its first read after a failover sees every write acknowledged before
//! it. It logs `singleton waiting` meanwhile.
//!
//! A singleton held but not running is checked as a start would be (its
//! dependencies, its supervision), so a node that does not run it refuses
//! what the primary would. A start that still fails when the node may run
//! singletons (a dependency stopped meanwhile) is tried again, with
//! backoff, until it succeeds or the leadership changes.
//!
//! A singleton in `snapshot` upgrade mode, on a node that runs
//! replication, moves with its state in a planned switchover
//! (`paddock_host`'s `handover`): [`Gate::hand_over`] takes its snapshot
//! and writes it to the replicated log, and every start of it takes the
//! record first (deleting it, committed), loading it in place of `init`
//! when there is one. Such a singleton also waits for replication before
//! it starts, so the record read is current.
//!
//! Each start logs `singleton starting` before the app's `init` runs and
//! `singleton started` or `cannot start singleton` after; each kill logs
//! `singleton killed` once it is done, and each graceful stop for a planned
//! switchover `singleton stopped`. `scripts/soak.sh` reads these records to
//! check that no two nodes run a singleton at once.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use paddock_bundle::manifest::{Durability, PlacementMode, UpgradeMode};
use paddock_host::failover::{Leadership, Standing};
use paddock_host::{
    AppDeployment, AppHandle, Apps, HostError, ReplicationStatus, Runtime, UpgradeError,
};
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
    rt: Runtime,
    apps: Apps,
    /// One start, stop or change of leadership at a time.
    op: tokio::sync::Mutex<()>,
    held: Mutex<Vec<Held>>,
    may_run: AtomicBool,
    /// Whether replication has this node's copy current (R5).
    ready: AtomicBool,
    /// The node's `cluster` shared buckets.
    cluster: BTreeSet<String>,
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
    /// The gate of a node whose `cluster` shared buckets are `cluster`.
    pub(super) fn new(rt: Runtime, apps: Apps, cluster: BTreeSet<String>) -> Self {
        Self {
            rt,
            apps,
            op: tokio::sync::Mutex::new(()),
            held: Mutex::default(),
            may_run: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            cluster,
            closed: AtomicBool::new(false),
        }
    }

    /// Whether `deployment` must wait for replication before it starts:
    /// it opens a replicated bucket or moves with its state, and this
    /// node's copy may trail.
    fn waits(&self, deployment: &AppDeployment) -> bool {
        let kv = &deployment.spec.services.keyvalue;
        let replicated = kv
            .buckets
            .values()
            .any(|b| b.durability == Durability::Replicated)
            || kv.shared.keys().any(|b| self.cluster.contains(b));
        (replicated || self.moves(deployment)) && !self.ready.load(Ordering::Relaxed)
    }

    /// Whether `deployment` moves with its state in a planned switchover:
    /// `snapshot` upgrade mode, on a node that runs replication.
    fn moves(&self, deployment: &AppDeployment) -> bool {
        deployment.manifest.spec.upgrade.mode == UpgradeMode::Snapshot && self.rt.moves_state()
    }

    /// Start `name`: from its handover record if it moves and one is
    /// kept, which this deletes first.
    async fn start(&self, name: &str, deployment: AppDeployment) -> Result<AppHandle, HostError> {
        if !self.moves(&deployment) {
            return self.apps.add(deployment).await;
        }
        match self.rt.take_handover(name).await? {
            Some(handover) => {
                tracing::info!(app = %name, from_version = %handover.version, "singleton moved in");
                self.apps.add_moved(deployment, handover).await
            }
            None => self.apps.add(deployment).await,
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

    /// The held singletons' current deployments, in deploy order.
    pub(super) fn deployments(&self) -> Vec<AppDeployment> {
        self.held().iter().map(|h| h.current.clone()).collect()
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
        if self.waits(&deployment) {
            tracing::info!(app = %name, "singleton waiting: replicated buckets catching up");
            return Ok(None);
        }
        tracing::info!(app = %name, "singleton starting: this node is primary");
        let added = self.start(name, deployment).await;
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
        if let Some(h) = self.held().iter().find(|h| h.name == name)
            && self.moves(&h.current)
        {
            // A later deploy of the app starts afresh.
            let rt = self.rt.clone();
            let app = name.to_string();
            tokio::spawn(async move {
                if let Err(e) = rt.take_handover(&app).await {
                    tracing::warn!(%app, error = %e, "handover record of a removed app not deleted");
                }
            });
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
                if self.apps.kill(name).await {
                    tracing::info!(app = %name, "singleton killed: this node may not run it");
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
            if self.waits(&current) {
                tracing::info!(app = %name, "singleton waiting: replicated buckets catching up");
                continue;
            }
            tracing::info!(app = %name, "singleton starting: this node is primary");
            match self.start(&name, current).await {
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
    /// a start that failed, to try again. One waiting for replication
    /// starts when replication is ready ([`Gate::set_ready`]).
    fn missing(&self) -> bool {
        self.may_run.load(Ordering::Relaxed)
            && !self.closed.load(Ordering::Relaxed)
            && self
                .held()
                .iter()
                .any(|h| self.apps.app(&h.name).is_none() && !self.waits(&h.current))
    }

    /// Record whether replication has this node's copy current, and start
    /// the singletons that waited for it. A singleton already running is
    /// not stopped when it stops being current: only `may_run` does that.
    async fn set_ready(&self, ready: bool) {
        let _op = self.op.lock().await;
        if self.ready.swap(ready, Ordering::Relaxed) == ready || !ready {
            return;
        }
        if self.may_run.load(Ordering::Relaxed) && !self.closed.load(Ordering::Relaxed) {
            self.start_held().await;
        }
    }

    /// Try the failed starts again, if this node may still run singletons.
    async fn retry(&self) {
        let _op = self.op.lock().await;
        if self.may_run.load(Ordering::Relaxed) && !self.closed.load(Ordering::Relaxed) {
            self.start_held().await;
        }
    }

    /// Stop the running singletons, in reverse deploy order, for a planned
    /// switchover, while the lease still holds: one that moves with its
    /// state writes its snapshot to the replicated log, the others run
    /// `terminate`. None starts again until the leadership allows it anew
    /// ([`Gate::restore`] if the switchover is refused after all, which
    /// starts the moved ones from their records).
    pub(super) async fn hand_over(&self) {
        let _op = self.op.lock().await;
        self.may_run.store(false, Ordering::Relaxed);
        let held: Vec<(String, bool)> = self
            .held()
            .iter()
            .map(|h| (h.name.clone(), self.moves(&h.current)))
            .collect();
        for (name, moves) in held.iter().rev() {
            if *moves && let Some(handover) = self.apps.move_out(name).await {
                let bytes = handover.snapshot.len();
                match self.rt.put_handover(name, &handover).await {
                    Ok(()) => tracing::info!(
                        app = %name,
                        bytes,
                        "singleton stopped: planned switchover, state moved"
                    ),
                    Err(e) => tracing::warn!(
                        app = %name,
                        error = %e,
                        "singleton stopped: planned switchover; state not moved, the next start runs init"
                    ),
                }
                continue;
            }
            if self.apps.take_out(name).await {
                tracing::info!(app = %name, "singleton stopped: planned switchover");
            }
        }
    }

    /// After a switchover refused once the singletons were stopped: start
    /// them again if the leadership still allows.
    pub(super) async fn restore(&self, may_run: bool) {
        self.set(may_run).await;
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
    let mut replication = rt.replication_updates();
    loop {
        let now = leadership.borrow_and_update().clone();
        gate.set_ready(ready_at(replication.as_mut(), now.epoch))
            .await;
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
                    Some(false)
                }
                () = replication_changed(replication.as_mut()) => None,
                _ = stop.wait_for(|s| *s) => return,
                () = tokio::time::sleep(wait), if retry => Some(true),
            };
            match woke {
                // Replication's status changed: start what waited for it.
                None => {
                    gate.set_ready(ready_at(replication.as_mut(), now.epoch))
                        .await;
                    continue;
                }
                Some(false) => break,
                Some(true) => {}
            }
            gate.retry().await;
            wait = (wait * 2).min(RETRY_MAX);
        }
    }
}

/// Whether replication, if it runs, has this node's copy current at
/// `epoch`. Without replication there is nothing to wait for.
fn ready_at(replication: Option<&mut watch::Receiver<ReplicationStatus>>, epoch: u64) -> bool {
    replication.is_none_or(|r| {
        let s = r.borrow_and_update();
        s.ready && s.epoch == epoch
    })
}

/// The next change of replication's status; never, without replication or
/// once it has stopped.
async fn replication_changed(replication: Option<&mut watch::Receiver<ReplicationStatus>>) {
    let Some(r) = replication else {
        return std::future::pending().await;
    };
    if r.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}
