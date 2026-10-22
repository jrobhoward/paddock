//! Running a set of apps: start order from `dependencies`, a root supervisor
//! per app, instance pools, and stop in reverse order.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use paddock_bundle::manifest::{AppManifest, Capability, Strategy, UpgradeMode};
use tokio::sync::watch;

use crate::error::HostError;
use crate::handover::Handover;
use crate::instance::InstanceHandle;
use crate::message::{CallError, Envelope, ExitReason};
use crate::pid::Pid;
use crate::pool::Pool;
use crate::probe;
use crate::runtime::{LoadedApp, Runtime};
use crate::spec::InstanceSpec;
use crate::supervisor::{
    Backoff, ChildHandle, ChildSpec, ChildStart, SupervisorHandle, SupervisorSpec,
};
use crate::upgrade::{UpgradeError, validate_blue_green, validate_upgrade};

/// An app ready to run: its verified manifest, compiled component, and the
/// spec its instances run with.
#[derive(Debug, Clone)]
pub struct AppDeployment {
    /// The manifest.
    pub manifest: AppManifest,
    /// The compiled component.
    pub app: LoadedApp,
    /// Limits, grants, buckets, config and preopens for each instance. The
    /// node narrows grants to the operator's and resolves preopens and
    /// config overrides here.
    pub spec: InstanceSpec,
}

impl AppDeployment {
    /// A deployment with the spec the manifest describes
    /// ([`InstanceSpec::from_manifest`]).
    #[must_use]
    pub fn new(manifest: AppManifest, app: LoadedApp) -> Self {
        let spec = InstanceSpec::from_manifest(&manifest);
        Self {
            manifest,
            app,
            spec,
        }
    }
}

/// Where an app is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStatus {
    /// Starting its instances.
    Starting,
    /// Every instance started.
    Running,
    /// The app could not start, or its root supervisor gave up. The reason
    /// says why (an escalation names the last child exit).
    Failed(ExitReason),
    /// Stopped by [`Apps::stop`].
    Stopped,
}

/// A running set of apps. Cheap to clone; clones share the set.
#[derive(Debug, Clone)]
pub struct Apps {
    inner: Arc<RwLock<AppSet>>,
    rt: Runtime,
}

#[derive(Debug, Default)]
struct AppSet {
    order: Vec<String>,
    apps: HashMap<String, AppHandle>,
}

/// One running app. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AppHandle {
    shared: Arc<AppShared>,
}

#[derive(Debug)]
struct AppShared {
    name: String,
    /// The current version's root supervisor; replaced when a blue-green
    /// upgrade promotes the new side.
    supervisor: Mutex<Option<SupervisorHandle>>,
    pool: Option<Arc<Pool>>,
    status: watch::Sender<AppStatus>,
    stopping: AtomicBool,
    versions: Mutex<Versions>,
    upgrading: AtomicBool,
    /// The new version running beside the current one in a blue-green
    /// upgrade.
    new_side: Mutex<Option<NewSide>>,
    /// Why the last new side ended without being promoted or dropped.
    new_failed: Mutex<Option<String>>,
}

/// The new version of a blue-green upgrade and its instances.
#[derive(Debug, Clone)]
struct NewSide {
    deployment: AppDeployment,
    supervisor: SupervisorHandle,
    approved: Vec<Capability>,
}

/// The new side of a blue-green upgrade, as [`AppHandle::new_side`]
/// reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NewSideInfo {
    /// Its version.
    pub version: semver::Version,
    /// Its share of the app's messages, 0 to 100.
    pub percent: u8,
    /// Restarts by its root supervisor since it started.
    pub restarts: u64,
}

#[derive(Debug)]
struct Versions {
    current: AppDeployment,
    previous: Option<AppDeployment>,
}

/// How an upgrade rolls out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeOptions {
    /// Approve capabilities the new version declares and the old one did
    /// not.
    pub approve: bool,
    /// Instances upgraded per batch (at least 1).
    pub batch_size: usize,
    /// Time to watch each batch before the next.
    pub observe: Duration,
    /// Most supervisor restarts tolerated during the rollout; one more stops
    /// it and reverts.
    pub max_restarts: u64,
    /// Move every instance in `restart` mode, whatever the target manifest's
    /// `upgrade.mode` says: stop it gracefully and start the other version
    /// from `init`, discarding in-memory state. For a rollback whose
    /// previous version cannot read the running version's snapshot.
    pub restart: bool,
}

impl Default for UpgradeOptions {
    fn default() -> Self {
        Self {
            approve: false,
            batch_size: 1,
            observe: Duration::ZERO,
            max_restarts: 0,
            restart: false,
        }
    }
}

/// What an upgrade or rollback did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeReport {
    /// The version before.
    pub from: semver::Version,
    /// The version after.
    pub to: semver::Version,
    /// How the instances moved.
    pub mode: UpgradeMode,
    /// The upgraded instances (new pids in `restart` mode).
    pub instances: Vec<Pid>,
    /// How long each instance paused (`snapshot` mode).
    pub pauses: Vec<Duration>,
    /// Capabilities approved for the new version.
    pub approved: Vec<Capability>,
}

impl AppHandle {
    /// The app's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// The app's status.
    #[must_use]
    pub fn status(&self) -> AppStatus {
        self.shared.status.borrow().clone()
    }

    /// A receiver that sees every status change.
    #[must_use]
    pub fn subscribe_status(&self) -> watch::Receiver<AppStatus> {
        self.shared.status.subscribe()
    }

    /// The running version.
    #[must_use]
    pub fn version(&self) -> semver::Version {
        self.versions().current.manifest.app.version.clone()
    }

    /// Set the version a rollback returns to, as when a node restores its
    /// deploy store after a restart.
    pub fn set_previous(&self, previous: Option<AppDeployment>) {
        self.versions().previous = previous;
    }

    /// The running deployment.
    #[must_use]
    pub fn deployment(&self) -> AppDeployment {
        self.versions().current.clone()
    }

    /// The deployment a rollback would return to.
    #[must_use]
    pub fn previous(&self) -> Option<AppDeployment> {
        self.versions().previous.clone()
    }

    /// The version a rollback would return to.
    #[must_use]
    pub fn previous_version(&self) -> Option<semver::Version> {
        self.versions()
            .previous
            .as_ref()
            .map(|d| d.manifest.app.version.clone())
    }

    /// How a rollback without [`UpgradeOptions::restart`] would move the
    /// instances; `None` without a previous version.
    #[must_use]
    pub fn rollback_mode(&self) -> Option<UpgradeMode> {
        let v = self.versions();
        let previous = v.previous.as_ref()?;
        Some(rollback_mode(&v.current, previous))
    }

    fn versions(&self) -> std::sync::MutexGuard<'_, Versions> {
        self.shared
            .versions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The app's root supervisor (the current version's); `None` if it
    /// never started.
    #[must_use]
    pub fn supervisor(&self) -> Option<SupervisorHandle> {
        self.shared
            .supervisor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The new version running beside the current one, in a blue-green
    /// upgrade.
    #[must_use]
    pub fn new_side(&self) -> Option<NewSideInfo> {
        let side = self.side()?;
        Some(NewSideInfo {
            version: side.deployment.manifest.app.version.clone(),
            percent: self
                .shared
                .pool
                .as_ref()
                .and_then(|p| p.percent())
                .unwrap_or(0),
            restarts: side.supervisor.restart_count(),
        })
    }

    /// The new side's deployment, in a blue-green upgrade.
    #[must_use]
    pub fn new_deployment(&self) -> Option<AppDeployment> {
        self.side().map(|s| s.deployment)
    }

    /// Why the last new side of a blue-green upgrade ended by itself (its
    /// root supervisor gave up), if one did.
    #[must_use]
    pub fn new_side_failure(&self) -> Option<String> {
        self.shared
            .new_failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn side(&self) -> Option<NewSide> {
        self.shared
            .new_side
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_side(&self, side: Option<NewSide>) {
        *self
            .shared
            .new_side
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = side;
    }

    fn is_current(&self, supervisor: &SupervisorHandle) -> bool {
        self.supervisor().is_some_and(|s| s.same(supervisor))
    }

    /// The app's running instances, in pool order.
    #[must_use]
    pub fn instances(&self) -> Vec<InstanceHandle> {
        self.shared
            .pool
            .as_ref()
            .map(|p| p.members())
            .unwrap_or_default()
    }

    /// The instance `msg` routes to under `instances.routing`.
    #[must_use]
    pub fn route(&self, msg: &Envelope) -> Option<InstanceHandle> {
        self.shared.pool.as_ref()?.pick(Some(msg))
    }

    /// Call the instance `msg` routes to.
    ///
    /// # Errors
    ///
    /// [`CallError::Noproc`] if no instance is running, and as for
    /// [`InstanceHandle::call`].
    pub async fn call(&self, msg: Envelope, timeout: Duration) -> Result<Envelope, CallError> {
        let target = self.route(&msg).ok_or(CallError::Noproc)?;
        target.call(msg, timeout).await
    }

    /// Cast to the instance `msg` routes to.
    ///
    /// # Errors
    ///
    /// [`CallError::Noproc`] if no instance is running, and as for
    /// [`InstanceHandle::cast`].
    pub fn cast(&self, msg: Envelope) -> Result<(), CallError> {
        self.route(&msg).ok_or(CallError::Noproc)?.cast(msg)
    }
}

impl Apps {
    fn set(&self) -> std::sync::RwLockReadGuard<'_, AppSet> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// App names in start order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.set().order.clone()
    }

    /// The runtime the apps run in, for the HTTP ingress.
    #[cfg(feature = "http")]
    pub(crate) fn runtime(&self) -> &Runtime {
        &self.rt
    }

    /// The app called `name`.
    #[must_use]
    pub fn app(&self, name: &str) -> Option<AppHandle> {
        self.set().apps.get(name).cloned()
    }

    /// The status of the app called `name`.
    #[must_use]
    pub fn status(&self, name: &str) -> Option<AppStatus> {
        self.app(name).map(|a| a.status())
    }

    /// Start one more app in this set. Its dependencies must be in the set
    /// and running, at matching versions.
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidApps`] for a name already in the set, a missing,
    /// stopped or mismatched dependency, or a supervision child naming an app
    /// that is not in the set. An app that starts but then fails is returned
    /// with [`AppStatus::Failed`], not as an error.
    pub async fn add(&self, deployment: AppDeployment) -> Result<AppHandle, HostError> {
        let name = deployment.manifest.app.name.clone();
        let spec = self.checked(&deployment, &[])?;
        let handle = start_app(&self.rt, &deployment, spec).await;
        let mut set = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        set.order.push(name.clone());
        set.apps.insert(name, handle.clone());
        Ok(handle)
    }

    /// Whether [`Apps::add`] would accept `deployment` once the apps in
    /// `also` are added too, without starting anything: for an app this
    /// node holds to start later (a singleton on a node that does not run
    /// singletons now), with `also` the others it holds, which count as
    /// deployed and running.
    ///
    /// # Errors
    ///
    /// As for [`Apps::add`].
    pub fn check_add(
        &self,
        deployment: &AppDeployment,
        also: &[AppDeployment],
    ) -> Result<(), HostError> {
        self.checked(deployment, also).map(|_| ())
    }

    /// [`Apps::add`]'s checks, and the root spec it would start.
    fn checked(
        &self,
        deployment: &AppDeployment,
        also: &[AppDeployment],
    ) -> Result<SupervisorSpec, HostError> {
        let name = deployment.manifest.app.name.as_str();
        let set = self.set();
        if set.apps.contains_key(name) {
            return Err(HostError::InvalidApps(format!(
                "{name} is already deployed"
            )));
        }
        let also_named = |app: &str| also.iter().find(|d| d.manifest.app.name == app);
        for dep in &deployment.manifest.spec.dependencies {
            let (version, running) = match (set.apps.get(&dep.app), also_named(&dep.app)) {
                (Some(found), _) => (found.version(), found.status() == AppStatus::Running),
                (None, Some(held)) => (held.manifest.app.version.clone(), true),
                (None, None) => {
                    return Err(HostError::InvalidApps(format!(
                        "{name} depends on {}, which is not deployed",
                        dep.app
                    )));
                }
            };
            if !dep.version.matches(&version) {
                return Err(HostError::InvalidApps(format!(
                    "{name} needs {} {}, but {version} is deployed",
                    dep.app, dep.version
                )));
            }
            if !running {
                return Err(HostError::InvalidApps(format!(
                    "dependency {} is not running",
                    dep.app
                )));
            }
        }
        let others: Vec<AppDeployment> = set.apps.values().map(AppHandle::deployment).collect();
        let mut by_name: HashMap<&str, &AppDeployment> = others
            .iter()
            .chain(also)
            .map(|d| (d.manifest.app.name.as_str(), d))
            .collect();
        by_name.insert(name, deployment);
        root_spec(deployment, &by_name).map_err(HostError::InvalidApps)
    }

    /// Kill every instance of `name` at once (no `terminate`, no restart)
    /// and remove it from the set, whatever depends on it: for a singleton
    /// that must stop now (its failover lease is ending). Returns whether
    /// `name` was in the set; one that is not has nothing left to kill.
    pub async fn kill(&self, name: &str) -> bool {
        let Some(app) = self.set().apps.get(name).cloned() else {
            return false;
        };
        end_app(&self.rt, &app, true).await;
        let mut set = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        set.order.retain(|n| n != name);
        set.apps.remove(name);
        true
    }

    /// Stop every instance of `name` gracefully (each runs `terminate`
    /// within its shutdown timeout; no restart) and take it out of the set,
    /// whatever depends on it: for a singleton handed over in a planned
    /// switchover, while its lease still holds. Returns whether `name` was
    /// in the set.
    pub async fn take_out(&self, name: &str) -> bool {
        let Some(app) = self.set().apps.get(name).cloned() else {
            return false;
        };
        end_app(&self.rt, &app, false).await;
        let mut set = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        set.order.retain(|n| n != name);
        set.apps.remove(name);
        true
    }

    /// Take `name` out of the set with its state, for a singleton moving
    /// to the other node of an HA pair (see [`Handover`]): its one
    /// instance writes its snapshot and handles nothing more, then the app
    /// stops, the instance exiting `normal` without `terminate`. `None`
    /// if `name` is not in the set, is not in `snapshot` upgrade mode, has
    /// other than one instance, or its `save-snapshot` fails; the app is
    /// then still in the set, for [`Apps::take_out`].
    pub async fn move_out(&self, name: &str) -> Option<Handover> {
        let app = self.set().apps.get(name).cloned()?;
        let deployment = app.deployment();
        if deployment.manifest.spec.upgrade.mode != UpgradeMode::Snapshot {
            return None;
        }
        let [instance] = app.instances().try_into().ok()?;
        let snapshot = match instance.move_out().await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                tracing::warn!(app = name, error = %e, "cannot move singleton's state");
                return None;
            }
        };
        end_app(&self.rt, &app, false).await;
        let mut set = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        set.order.retain(|n| n != name);
        set.apps.remove(name);
        Some(Handover::new(
            deployment.manifest.app.version.to_string(),
            snapshot,
        ))
    }

    /// [`Apps::add`], with the app's first instance loading `handover`'s
    /// snapshot in place of `init` (a singleton moved here). A version
    /// whose manifest does not accept the snapshot's (`upgrade.from_versions`,
    /// or other than `snapshot` mode) starts from `init`, as does an
    /// instance whose `load-snapshot` refuses it, on its supervisor's
    /// restart.
    ///
    /// # Errors
    ///
    /// As for [`Apps::add`].
    pub async fn add_moved(
        &self,
        deployment: AppDeployment,
        handover: Handover,
    ) -> Result<AppHandle, HostError> {
        let name = deployment.manifest.app.name.clone();
        if accepts_snapshot(&deployment, &handover.version) {
            self.rt.inner().handovers.put(&name, handover);
        } else {
            tracing::warn!(
                app = %name,
                from_version = %handover.version,
                "moved snapshot not accepted by this version; starting from init"
            );
        }
        let added = self.add(deployment).await;
        // Not taken if no instance started.
        self.rt.inner().handovers.take(&name);
        added
    }

    /// Stop an app and take it out of the set.
    ///
    /// # Errors
    ///
    /// [`HostError::InvalidApps`] for an app not in the set, or one that
    /// running apps depend on.
    pub async fn remove(&self, name: &str) -> Result<(), HostError> {
        let app = {
            let set = self.set();
            let app = set
                .apps
                .get(name)
                .cloned()
                .ok_or_else(|| HostError::InvalidApps(format!("{name} is not deployed")))?;
            let dependents: Vec<&str> = set
                .apps
                .values()
                .filter(|a| a.status() == AppStatus::Running)
                .filter(|a| {
                    a.deployment()
                        .manifest
                        .spec
                        .dependencies
                        .iter()
                        .any(|d| d.app == name)
                })
                .map(AppHandle::name)
                .collect();
            if !dependents.is_empty() {
                return Err(HostError::InvalidApps(format!(
                    "{name} is needed by {}",
                    dependents.join(", ")
                )));
            }
            app
        };
        stop_app(&self.rt, &app).await;
        let mut set = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        set.order.retain(|n| n != name);
        set.apps.remove(name);
        Ok(())
    }

    /// Move a running app to `new`, as its manifest's `upgrade.mode` says
    /// (or in `restart` mode with [`UpgradeOptions::restart`]):
    /// `snapshot` carries each instance's state across (same pids), `restart`
    /// stops each instance and starts the new version from `init` (same ids,
    /// next incarnation). Instances move in batches; a failed instance, or
    /// more than `max_restarts` supervisor restarts, stops the rollout and
    /// returns every moved instance to the old version.
    ///
    /// # Errors
    ///
    /// The validation errors of [`validate_upgrade`];
    /// [`UpgradeError::UnknownApp`], [`UpgradeError::InProgress`],
    /// [`UpgradeError::Invalid`] for an app that is not running; and
    /// [`UpgradeError::Reverted`] when the rollout stopped.
    pub async fn upgrade(
        &self,
        name: &str,
        new: AppDeployment,
        options: &UpgradeOptions,
    ) -> Result<UpgradeReport, UpgradeError> {
        let app = self
            .app(name)
            .ok_or_else(|| UpgradeError::UnknownApp(name.into()))?;
        if new.manifest.spec.upgrade.mode == UpgradeMode::BlueGreen && !options.restart {
            return Err(UpgradeError::Invalid(format!(
                "{name} {} upgrades blue-green: start it beside the running version",
                new.manifest.app.version
            )));
        }
        let old = app.versions().current.clone();
        let approved = validate_upgrade(&old.manifest, &new.manifest, options.approve)?;
        let mode = if options.restart {
            UpgradeMode::Restart
        } else {
            new.manifest.spec.upgrade.mode
        };
        let mut report = move_app(&app, &old, &new, mode, options).await?;
        report.approved = approved;
        let mut versions = app.versions();
        versions.previous = Some(std::mem::replace(&mut versions.current, new));
        Ok(report)
    }

    /// Return a running app to its previous version. Uses `snapshot` mode if
    /// the previous version's manifest asks for it and accepts snapshots
    /// from the running version, otherwise `restart`
    /// ([`AppHandle::rollback_mode`]); [`UpgradeOptions::restart`] forces
    /// `restart`. Capabilities are not re-approved: the previous version ran
    /// before.
    ///
    /// # Errors
    ///
    /// [`UpgradeError::NoPrevious`], and as for [`Apps::upgrade`].
    pub async fn rollback(
        &self,
        name: &str,
        options: &UpgradeOptions,
    ) -> Result<UpgradeReport, UpgradeError> {
        let app = self
            .app(name)
            .ok_or_else(|| UpgradeError::UnknownApp(name.into()))?;
        let (current, previous) = {
            let v = app.versions();
            let previous = v
                .previous
                .clone()
                .ok_or_else(|| UpgradeError::NoPrevious(name.into()))?;
            (v.current.clone(), previous)
        };
        let mode = if options.restart {
            UpgradeMode::Restart
        } else {
            rollback_mode(&current, &previous)
        };
        let report = move_app(&app, &current, &previous, mode, options).await?;
        let mut versions = app.versions();
        let back = std::mem::replace(&mut versions.current, previous);
        versions.previous = Some(back);
        Ok(report)
    }

    /// Start `new` beside the running version of app `name`, for a
    /// blue-green upgrade (DESIGN.md, "Blue-green upgrades"): its own root
    /// supervisor and instances, in the app's pool at no traffic. Other
    /// upgrades of the app are refused until it is promoted or dropped.
    /// Returns the capabilities approved for it.
    ///
    /// # Errors
    ///
    /// The errors of [`validate_blue_green`]; [`UpgradeError::UnknownApp`],
    /// [`UpgradeError::InProgress`], [`UpgradeError::Invalid`] for an app
    /// that is not running; [`UpgradeError::Instantiate`] if the new
    /// version's instances do not start.
    pub async fn start_new(
        &self,
        name: &str,
        new: AppDeployment,
        approve: bool,
    ) -> Result<Vec<Capability>, UpgradeError> {
        let app = self
            .app(name)
            .ok_or_else(|| UpgradeError::UnknownApp(name.into()))?;
        let pool = app
            .shared
            .pool
            .clone()
            .filter(|_| app.status() == AppStatus::Running)
            .ok_or_else(|| UpgradeError::Invalid(format!("{name} is not running")))?;
        let old = app.versions().current.clone();
        let approved = validate_blue_green(&old.manifest, &new.manifest, approve)?;
        if app
            .shared
            .upgrading
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(UpgradeError::InProgress(name.into()));
        }
        let started = start_side(&self.rt, &new).await;
        let supervisor = match started {
            Ok(s) => s,
            Err(why) => {
                app.shared.upgrading.store(false, Ordering::Release);
                return Err(UpgradeError::Instantiate(why));
            }
        };
        *app.shared
            .new_failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        app.set_side(Some(NewSide {
            deployment: new.clone(),
            supervisor: supervisor.clone(),
            approved: approved.clone(),
        }));
        pool.add_new(supervisor.clone());
        tokio::spawn(watch_side(app.clone(), supervisor));
        tracing::info!(app = %name, from = %old.manifest.app.version, to = %new.manifest.app.version, "new version started beside the old");
        Ok(approved)
    }

    /// Give the new side of app `name`'s blue-green upgrade `percent` of
    /// its messages (at most 100).
    ///
    /// # Errors
    ///
    /// [`UpgradeError::UnknownApp`]; [`UpgradeError::Invalid`] if no new
    /// side runs.
    pub fn set_traffic(&self, name: &str, percent: u8) -> Result<(), UpgradeError> {
        let (app, pool, _) = self.with_new_side(name)?;
        pool.set_percent(percent);
        tracing::info!(app = %app.name(), percent = percent.min(100), "traffic to the new version");
        Ok(())
    }

    /// Finish app `name`'s blue-green upgrade: every message to the new
    /// side, the old side's instances given up to `drain` to empty their
    /// mailboxes, then stopped; the new version becomes current and the old
    /// one previous.
    ///
    /// # Errors
    ///
    /// [`UpgradeError::UnknownApp`]; [`UpgradeError::Invalid`] if no new
    /// side runs.
    pub async fn promote(
        &self,
        name: &str,
        drain: Duration,
    ) -> Result<UpgradeReport, UpgradeError> {
        let (app, pool, side) = self.with_new_side(name)?;
        pool.set_percent(100);
        let old_supervisor = app.supervisor();
        if let Some(old) = &old_supervisor {
            drain_side(old, drain).await;
        }
        let from = {
            let mut versions = app.versions();
            let from = versions.current.manifest.app.version.clone();
            versions.previous = Some(std::mem::replace(
                &mut versions.current,
                side.deployment.clone(),
            ));
            from
        };
        *app.shared
            .supervisor
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(side.supervisor.clone());
        pool.promote();
        app.set_side(None);
        if let Some(old) = old_supervisor {
            old.shutdown();
            old.wait().await;
        }
        app.shared.upgrading.store(false, Ordering::Release);
        let to = side.deployment.manifest.app.version.clone();
        tracing::info!(app = %name, %from, %to, "blue-green upgrade promoted");
        Ok(UpgradeReport {
            from,
            to,
            mode: UpgradeMode::BlueGreen,
            instances: app.instances().iter().map(InstanceHandle::pid).collect(),
            pauses: Vec::new(),
            approved: side.approved,
        })
    }

    /// End app `name`'s blue-green upgrade without it: no messages to the
    /// new side, its instances given up to `drain` to empty their
    /// mailboxes, then stopped. Returns the new side's version.
    ///
    /// # Errors
    ///
    /// [`UpgradeError::UnknownApp`]; [`UpgradeError::Invalid`] if no new
    /// side runs.
    pub async fn drop_new(
        &self,
        name: &str,
        drain: Duration,
    ) -> Result<semver::Version, UpgradeError> {
        let (app, pool, side) = self.with_new_side(name)?;
        pool.set_percent(0);
        drain_side(&side.supervisor, drain).await;
        // Out of the side first, so its watcher takes the stop as meant.
        app.set_side(None);
        pool.remove_new();
        side.supervisor.shutdown();
        side.supervisor.wait().await;
        app.shared.upgrading.store(false, Ordering::Release);
        let version = side.deployment.manifest.app.version.clone();
        tracing::info!(app = %name, %version, "blue-green upgrade dropped");
        Ok(version)
    }

    fn with_new_side(&self, name: &str) -> Result<(AppHandle, Arc<Pool>, NewSide), UpgradeError> {
        let app = self
            .app(name)
            .ok_or_else(|| UpgradeError::UnknownApp(name.into()))?;
        let no_side =
            || UpgradeError::Invalid(format!("{name} has no new version running beside it"));
        let side = app.side().ok_or_else(no_side)?;
        let pool = app.shared.pool.clone().ok_or_else(no_side)?;
        Ok((app, pool, side))
    }

    /// Stop every app, in reverse start order, waiting for each root
    /// supervisor to stop its instances.
    pub async fn stop(&self) {
        let (order, apps) = {
            let set = self.set();
            (set.order.clone(), set.apps.clone())
        };
        for name in order.iter().rev() {
            if let Some(app) = apps.get(name) {
                stop_app(&self.rt, app).await;
            }
        }
    }
}

/// Whether `deployment` loads a snapshot written by `version`: in
/// `snapshot` mode, and `version` its own or one its `from_versions`
/// accepts.
fn accepts_snapshot(deployment: &AppDeployment, version: &str) -> bool {
    let manifest = &deployment.manifest;
    let upgrade = &manifest.spec.upgrade;
    if upgrade.mode != UpgradeMode::Snapshot {
        return false;
    }
    if manifest.app.version.to_string() == version {
        return true;
    }
    let Ok(version) = semver::Version::parse(version) else {
        return false;
    };
    upgrade.from_versions.is_empty() || upgrade.from_versions.iter().any(|r| r.matches(&version))
}

/// How a rollback from `current` to `previous` moves instances: `snapshot`
/// if `previous` asks for it and accepts the running version, else
/// `restart`.
fn rollback_mode(current: &AppDeployment, previous: &AppDeployment) -> UpgradeMode {
    let upgrade = &previous.manifest.spec.upgrade;
    let accepts = upgrade.from_versions.is_empty()
        || upgrade
            .from_versions
            .iter()
            .any(|r| r.matches(&current.manifest.app.version));
    if upgrade.mode == UpgradeMode::Snapshot && accepts {
        UpgradeMode::Snapshot
    } else {
        UpgradeMode::Restart
    }
}

/// Stop an app's root supervisor and mark it stopped (unless it failed).
async fn stop_app(rt: &Runtime, app: &AppHandle) {
    end_app(rt, app, false).await;
}

/// Stop `app`: gracefully, or with `kill` at once, every instance killed
/// and none restarted.
async fn end_app(rt: &Runtime, app: &AppHandle, kill: bool) {
    app.shared.stopping.store(true, Ordering::Relaxed);
    let new = app.side().map(|s| s.supervisor);
    for supervisor in app.supervisor().into_iter().chain(new) {
        if kill {
            supervisor.kill();
        } else {
            supervisor.shutdown();
        }
        supervisor.wait().await;
    }
    rt.remove_pool(app.name());
    if !matches!(app.status(), AppStatus::Failed(_)) {
        app.shared.status.send_replace(AppStatus::Stopped);
    }
}

/// Start `deployments` in dependency order. An app whose start fails, or
/// whose dependency is not running, is marked [`AppStatus::Failed`]; the
/// others still start.
pub(crate) async fn start(
    rt: &Runtime,
    deployments: Vec<AppDeployment>,
) -> Result<Apps, HostError> {
    let manifests: Vec<&AppManifest> = deployments.iter().map(|d| &d.manifest).collect();
    let order = start_order(&manifests).map_err(HostError::InvalidApps)?;
    let by_name: HashMap<&str, &AppDeployment> = deployments
        .iter()
        .map(|d| (d.manifest.app.name.as_str(), d))
        .collect();
    let mut specs = Vec::with_capacity(order.len());
    for &i in &order {
        specs.push(root_spec(&deployments[i], &by_name).map_err(HostError::InvalidApps)?);
    }

    let mut set = AppSet::default();
    for (&i, spec) in order.iter().zip(specs) {
        let deployment = &deployments[i];
        let name = deployment.manifest.app.name.clone();
        let blocked = deployment
            .manifest
            .spec
            .dependencies
            .iter()
            .find(|d| set.apps.get(&d.app).map(AppHandle::status) != Some(AppStatus::Running));
        let handle = match blocked {
            Some(dep) => failed(deployment, format!("dependency {} is not running", dep.app)),
            None => start_app(rt, deployment, spec).await,
        };
        set.order.push(name.clone());
        set.apps.insert(name, handle);
    }
    Ok(Apps {
        inner: Arc::new(RwLock::new(set)),
        rt: rt.clone(),
    })
}

fn failed(deployment: &AppDeployment, why: String) -> AppHandle {
    let name = &deployment.manifest.app.name;
    tracing::error!(app = %name, reason = %why, "app not started");
    AppHandle {
        shared: Arc::new(AppShared {
            name: name.clone(),
            supervisor: Mutex::new(None),
            pool: None,
            status: watch::channel(AppStatus::Failed(ExitReason::Crashed(why))).0,
            stopping: AtomicBool::new(false),
            versions: Mutex::new(Versions {
                current: deployment.clone(),
                previous: None,
            }),
            upgrading: AtomicBool::new(false),
            new_side: Mutex::new(None),
            new_failed: Mutex::new(None),
        }),
    }
}

async fn start_app(rt: &Runtime, deployment: &AppDeployment, spec: SupervisorSpec) -> AppHandle {
    let manifest = &deployment.manifest;
    let name = manifest.app.name.clone();
    let pooled = spec.strategy == Strategy::SimpleOneForOne;
    let budget = start_budget(&spec);
    let supervisor = match rt.start_supervisor(spec) {
        Ok(s) => s,
        Err(err) => return failed(deployment, err.to_string()),
    };
    if pooled {
        for _ in 0..manifest.spec.instances.initial() {
            if let Err(err) = supervisor.start_child().await {
                supervisor.shutdown();
                return failed(deployment, err.to_string());
            }
        }
    }
    let pool = Arc::new(Pool::new(
        name.clone(),
        manifest.spec.instances.routing.clone(),
        supervisor.clone(),
    ));
    let (status, _) = watch::channel(AppStatus::Starting);
    let handle = AppHandle {
        shared: Arc::new(AppShared {
            name: name.clone(),
            supervisor: Mutex::new(Some(supervisor.clone())),
            pool: Some(pool.clone()),
            status,
            stopping: AtomicBool::new(false),
            versions: Mutex::new(Versions {
                current: deployment.clone(),
                previous: None,
            }),
            upgrading: AtomicBool::new(false),
            new_side: Mutex::new(None),
            new_failed: Mutex::new(None),
        }),
    };
    match wait_started(&supervisor, budget).await {
        Ok(()) => {
            rt.add_pool(&name, pool);
            handle.shared.status.send_replace(AppStatus::Running);
            tracing::info!(app = %name, version = %manifest.app.version, "app running");
        }
        Err(reason) => {
            supervisor.shutdown();
            tracing::error!(app = %name, %reason, "app failed to start");
            handle.shared.status.send_replace(AppStatus::Failed(reason));
            return handle;
        }
    }
    tokio::spawn(watch_side(handle.clone(), supervisor));
    handle
}

/// Follow one of `app`'s root supervisors until it ends: the current
/// version's failing fails the app; the new side's failing (in a blue-green
/// upgrade) takes it out of the pool, and the old version carries on. A
/// supervisor that is neither any more was stopped on purpose.
async fn watch_side(app: AppHandle, supervisor: SupervisorHandle) {
    let reason = supervisor.wait().await;
    if app.shared.stopping.load(Ordering::Relaxed) {
        return;
    }
    if app.is_current(&supervisor) {
        tracing::error!(app = %app.shared.name, %reason, "app failed");
        app.shared.status.send_replace(AppStatus::Failed(reason));
        return;
    }
    let is_new = app.side().is_some_and(|s| s.supervisor.same(&supervisor));
    if is_new {
        tracing::error!(app = %app.shared.name, %reason, "new version failed; its traffic returns to the old");
        if let Some(pool) = &app.shared.pool {
            pool.remove_new();
        }
        app.set_side(None);
        *app.shared
            .new_failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(reason.to_string());
        app.shared.upgrading.store(false, Ordering::Release);
    }
}

/// Start the root supervisor and instances of `deployment` for the new side
/// of a blue-green upgrade, and wait until they run.
async fn start_side(rt: &Runtime, deployment: &AppDeployment) -> Result<SupervisorHandle, String> {
    let spec = root_spec(deployment, &HashMap::new())?;
    let pooled = spec.strategy == Strategy::SimpleOneForOne;
    let budget = start_budget(&spec);
    let supervisor = rt.start_supervisor(spec).map_err(|e| e.to_string())?;
    if pooled {
        for _ in 0..deployment.manifest.spec.instances.initial() {
            if let Err(e) = supervisor.start_child().await {
                supervisor.shutdown();
                return Err(e.to_string());
            }
        }
    }
    if let Err(reason) = wait_started(&supervisor, budget).await {
        supervisor.shutdown();
        return Err(reason.to_string());
    }
    Ok(supervisor)
}

/// Wait up to `drain` for the instances under `supervisor` to empty their
/// mailboxes, once no message to the app by name goes to them.
async fn drain_side(supervisor: &SupervisorHandle, drain: Duration) {
    let deadline = Instant::now() + drain;
    loop {
        let queued: usize = supervisor
            .children()
            .iter()
            .filter_map(|c| c.instance().map(InstanceHandle::queued))
            .sum();
        if queued == 0 || Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Clears an app's in-progress flag when an upgrade ends, however it ends.
struct UpgradeGuard<'a>(&'a AtomicBool);

impl Drop for UpgradeGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Move an app's own instances from `from` to `to`. Children that run other
/// apps (`supervision.children`) are left alone.
async fn move_app(
    app: &AppHandle,
    from: &AppDeployment,
    to: &AppDeployment,
    mode: UpgradeMode,
    options: &UpgradeOptions,
) -> Result<UpgradeReport, UpgradeError> {
    let version = to.manifest.app.version.to_string();
    probe::upgrade_start(app.name(), &version);
    let result = rollout(app, from, to, mode, options).await;
    probe::upgrade_done(app.name(), &version, result.is_ok());
    result
}

async fn rollout(
    app: &AppHandle,
    from: &AppDeployment,
    to: &AppDeployment,
    mode: UpgradeMode,
    options: &UpgradeOptions,
) -> Result<UpgradeReport, UpgradeError> {
    let name = app.name().to_string();
    let supervisor = app
        .supervisor()
        .filter(|_| app.status() == AppStatus::Running)
        .ok_or_else(|| UpgradeError::Invalid(format!("{name} is not running")))?;
    if app
        .shared
        .upgrading
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return Err(UpgradeError::InProgress(name));
    }
    let _guard = UpgradeGuard(&app.shared.upgrading);

    let rollout = Rollout::new(&supervisor, from, to, mode);
    let pooled = supervisor.strategy() == Strategy::SimpleOneForOne;
    if pooled {
        supervisor
            .replace_template(rollout.start(true))
            .await
            .map_err(|e| UpgradeError::Invalid(e.to_string()))?;
    }
    let ids: Vec<String> = supervisor
        .children()
        .iter()
        .filter(|c| c.app.as_deref() == Some(name.as_str()) && c.instance().is_some())
        .map(|c| c.id.clone())
        .collect();
    let base = supervisor.restart_count();
    let mut moved: Vec<String> = Vec::new();
    let mut report = UpgradeReport {
        from: from.manifest.app.version.clone(),
        to: to.manifest.app.version.clone(),
        mode,
        instances: Vec::new(),
        pauses: Vec::new(),
        approved: Vec::new(),
    };
    for batch in ids.chunks(options.batch_size.max(1)) {
        let mut failure = None;
        for id in batch {
            match rollout.move_one(id, true).await {
                Ok((pid, pause)) => {
                    moved.push(id.clone());
                    report.instances.push(pid);
                    report.pauses.extend(pause);
                }
                Err(err) => {
                    failure = Some(format!("{id}: {err}"));
                    break;
                }
            }
        }
        if failure.is_none() && !options.observe.is_zero() {
            tokio::time::sleep(options.observe).await;
        }
        let restarts = supervisor.restart_count().saturating_sub(base);
        if failure.is_none() && restarts > options.max_restarts {
            failure = Some(format!(
                "{restarts} restarts during the rollout (limit {})",
                options.max_restarts
            ));
        }
        if let Some(mut why) = failure {
            tracing::warn!(app = %name, reason = %why, "rollout stopped; reverting");
            for id in moved.iter().rev() {
                rollout.revert_one(id).await;
            }
            if pooled && let Err(e) = supervisor.replace_template(rollout.start(false)).await {
                tracing::error!(
                    app = %name,
                    error = %e,
                    "rollout reverted, but the pool template is still the new version"
                );
                why = format!(
                    "{why}; the pool template could not be put back ({e}), so instances \
                     started from now on run {}",
                    report.to
                );
            }
            return Err(UpgradeError::Reverted(why));
        }
    }
    tracing::info!(app = %name, from = %report.from, to = %report.to, ?mode, "upgraded");
    Ok(report)
}

/// One app's move between two versions.
struct Rollout<'a> {
    supervisor: &'a SupervisorHandle,
    from: &'a AppDeployment,
    to: &'a AppDeployment,
    mode: UpgradeMode,
}

impl<'a> Rollout<'a> {
    fn new(
        supervisor: &'a SupervisorHandle,
        from: &'a AppDeployment,
        to: &'a AppDeployment,
        mode: UpgradeMode,
    ) -> Self {
        Self {
            supervisor,
            from,
            to,
            mode,
        }
    }

    /// The child spec for the new (`forward`) or old version.
    fn start(&self, forward: bool) -> ChildStart {
        let d = if forward { self.to } else { self.from };
        ChildStart::Instance {
            app: d.app.clone(),
            spec: d.spec.clone(),
        }
    }

    fn instance(&self, id: &str) -> Option<InstanceHandle> {
        self.supervisor
            .children()
            .into_iter()
            .find(|c| c.id == id)
            .and_then(|c| c.instance().cloned())
    }

    /// Move one child forward or back. In `snapshot` mode the running
    /// instance is upgraded first and its spec replaced after, so an
    /// instance that dies in `save-snapshot` restarts as the version it was.
    async fn move_one(
        &self,
        id: &str,
        forward: bool,
    ) -> Result<(Pid, Option<Duration>), UpgradeError> {
        let (target, source) = if forward {
            (self.to, self.from)
        } else {
            (self.from, self.to)
        };
        let host = |e: crate::error::HostError| UpgradeError::Instantiate(e.to_string());
        match self.mode {
            UpgradeMode::Snapshot => {
                let instance = self.instance(id).ok_or(UpgradeError::NotRunning)?;
                let pause = instance
                    .upgrade(
                        &target.app,
                        target.spec.clone(),
                        &source.manifest.app.version.to_string(),
                    )
                    .await?;
                self.supervisor
                    .replace_child(id, self.start(forward), false)
                    .await
                    .map_err(host)?;
                Ok((instance.pid(), Some(pause)))
            }
            _ => {
                let child = self
                    .supervisor
                    .replace_child(id, self.start(forward), true)
                    .await
                    .map_err(host)?;
                Ok((child.pid().ok_or(UpgradeError::NotRunning)?, None))
            }
        }
    }

    /// Return a moved child to the old version: by snapshot if possible,
    /// otherwise by restarting it as the old version from `init`.
    async fn revert_one(&self, id: &str) {
        if self.move_one(id, false).await.is_err()
            && let Err(err) = self
                .supervisor
                .replace_child(id, self.start(false), true)
                .await
        {
            tracing::error!(child = %id, error = %err, "revert failed");
        }
    }
}

/// How long an app may take to bring every instance to `Active`: one init
/// timeout per allowed restart, plus the backoff between them.
fn start_budget(spec: &SupervisorSpec) -> Duration {
    let init = spec
        .children
        .iter()
        .filter_map(|c| match &c.start {
            ChildStart::Instance { spec, .. } => Some(spec.init_timeout),
            ChildStart::Supervisor(_) => None,
        })
        .max()
        .unwrap_or(Duration::from_secs(5));
    let attempts = spec.intensity.saturating_add(2);
    init.saturating_mul(attempts)
        + spec.backoff.max.saturating_mul(attempts)
        + Duration::from_secs(1)
}

/// Wait until every running instance child is `Active`, or the supervisor
/// gives up.
async fn wait_started(supervisor: &SupervisorHandle, budget: Duration) -> Result<(), ExitReason> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(reason) = supervisor.exit_reason() {
            return Err(reason);
        }
        // A child waiting out a backoff is not started; one that finished
        // for good (a transient that exited normally) does not hold up the
        // app.
        let all_active = supervisor.children().iter().all(|c| match &c.handle {
            Some(ChildHandle::Instance(h)) => h.state().handles_messages(),
            Some(ChildHandle::Supervisor(_)) => true,
            None => !c.restarting,
        });
        if all_active {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(ExitReason::Crashed(format!(
                "instances not active after {} ms",
                budget.as_millis()
            )));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The root supervisor for one app. `simple_one_for_one` uses
/// `supervision.children[0]` as the pool template; other strategies run
/// `instances.count` instances of the app itself, then the listed children.
fn root_spec(
    deployment: &AppDeployment,
    by_name: &HashMap<&str, &AppDeployment>,
) -> Result<SupervisorSpec, String> {
    let manifest = &deployment.manifest;
    let supervision = &manifest.spec.supervision;
    let mut children = Vec::new();
    if supervision.strategy != Strategy::SimpleOneForOne {
        for n in 1..=manifest.spec.instances.initial() {
            children.push(ChildSpec {
                id: format!("instance-{n}"),
                restart: manifest.spec.lifecycle.restart,
                start: ChildStart::Instance {
                    app: deployment.app.clone(),
                    spec: deployment.spec.clone(),
                },
            });
        }
    }
    for child in &supervision.children {
        let target = by_name.get(child.app.as_str()).ok_or_else(|| {
            format!(
                "{}: supervision child '{}' runs app '{}', which is not deployed",
                manifest.app.name, child.id, child.app
            )
        })?;
        let mut spec = target.spec.clone();
        if let Some(ms) = child.shutdown_timeout_ms {
            spec.shutdown_timeout = Duration::from_millis(ms);
        }
        if let Some(args) = &child.start_args {
            spec.init_args = args.to_string().into_bytes();
        }
        children.push(ChildSpec {
            id: child.id.clone(),
            restart: child
                .restart
                .unwrap_or(target.manifest.spec.lifecycle.restart),
            start: ChildStart::Instance {
                app: target.app.clone(),
                spec,
            },
        });
    }
    Ok(SupervisorSpec {
        name: manifest.app.name.clone(),
        strategy: supervision.strategy,
        intensity: supervision.intensity,
        period: Duration::from_millis(supervision.period_ms),
        backoff: Backoff::from(&supervision.backoff),
        children,
    })
}

/// Start order: each app after the apps it depends on, otherwise in the
/// order given. Returns indexes into `manifests`.
///
/// # Errors
///
/// A message naming a missing dependency, one whose deployed version does not
/// match the requirement, a duplicate app name, or a dependency cycle.
pub(crate) fn start_order(manifests: &[&AppManifest]) -> Result<Vec<usize>, String> {
    let index: HashMap<&str, usize> = manifests
        .iter()
        .enumerate()
        .map(|(i, m)| (m.app.name.as_str(), i))
        .collect();
    if index.len() != manifests.len() {
        return Err("two deployments have the same app name".to_string());
    }
    for m in manifests {
        for dep in &m.spec.dependencies {
            let Some(&i) = index.get(dep.app.as_str()) else {
                return Err(format!(
                    "{} depends on {}, which is not deployed",
                    m.app.name, dep.app
                ));
            };
            let found = &manifests[i].app.version;
            if !dep.version.matches(found) {
                return Err(format!(
                    "{} needs {} {}, but {found} is deployed",
                    m.app.name, dep.app, dep.version
                ));
            }
        }
    }
    let mut order = Vec::with_capacity(manifests.len());
    let mut started = vec![false; manifests.len()];
    while order.len() < manifests.len() {
        let next = (0..manifests.len()).find(|&i| {
            !started[i]
                && manifests[i]
                    .spec
                    .dependencies
                    .iter()
                    .all(|d| index.get(d.app.as_str()).is_some_and(|&j| started[j]))
        });
        let Some(i) = next else {
            let stuck: Vec<&str> = (0..manifests.len())
                .filter(|&i| !started[i])
                .map(|i| manifests[i].app.name.as_str())
                .collect();
            return Err(format!("dependency cycle among {}", stuck.join(", ")));
        };
        started[i] = true;
        order.push(i);
    }
    Ok(order)
}

#[cfg(test)]
#[path = "apps_tests.rs"]
mod apps_tests;
