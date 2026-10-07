//! Running a set of apps: start order from `dependencies`, a root supervisor
//! per app, instance pools, and stop in reverse order.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use paddock_manifest::{AppManifest, Strategy};
use tokio::sync::watch;

use crate::error::HostError;
use crate::instance::InstanceHandle;
use crate::lifecycle::LifecycleState;
use crate::message::{CallError, Envelope, ExitReason};
use crate::pool::Pool;
use crate::runtime::{LoadedApp, Runtime};
use crate::spec::InstanceSpec;
use crate::supervisor::{
    Backoff, ChildHandle, ChildSpec, ChildStart, SupervisorHandle, SupervisorSpec,
};

/// An app ready to run: its verified manifest and compiled component.
#[derive(Debug, Clone)]
pub struct AppDeployment {
    /// The manifest.
    pub manifest: AppManifest,
    /// The compiled component.
    pub app: LoadedApp,
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

/// A started set of apps.
#[derive(Debug, Clone)]
pub struct Apps {
    order: Vec<String>,
    apps: HashMap<String, AppHandle>,
    rt: Runtime,
}

/// One running app. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AppHandle {
    shared: Arc<AppShared>,
}

#[derive(Debug)]
struct AppShared {
    name: String,
    supervisor: Option<SupervisorHandle>,
    pool: Option<Arc<Pool>>,
    status: watch::Sender<AppStatus>,
    stopping: AtomicBool,
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

    /// The app's root supervisor; `None` if it never started.
    #[must_use]
    pub fn supervisor(&self) -> Option<&SupervisorHandle> {
        self.shared.supervisor.as_ref()
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
    /// App names in start order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.order
    }

    /// The app called `name`.
    #[must_use]
    pub fn app(&self, name: &str) -> Option<&AppHandle> {
        self.apps.get(name)
    }

    /// The status of the app called `name`.
    #[must_use]
    pub fn status(&self, name: &str) -> Option<AppStatus> {
        self.apps.get(name).map(AppHandle::status)
    }

    /// Stop every app, in reverse start order, waiting for each root
    /// supervisor to stop its instances.
    pub async fn stop(&self) {
        for name in self.order.iter().rev() {
            let Some(app) = self.apps.get(name) else {
                continue;
            };
            app.shared.stopping.store(true, Ordering::Relaxed);
            if let Some(supervisor) = &app.shared.supervisor {
                supervisor.shutdown();
                supervisor.wait().await;
            }
            self.rt.remove_pool(name);
            let failed = matches!(app.status(), AppStatus::Failed(_));
            if !failed {
                app.shared.status.send_replace(AppStatus::Stopped);
            }
        }
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

    let mut apps = Apps {
        order: Vec::new(),
        apps: HashMap::new(),
        rt: rt.clone(),
    };
    for (&i, spec) in order.iter().zip(specs) {
        let deployment = &deployments[i];
        let name = deployment.manifest.app.name.clone();
        let blocked = deployment
            .manifest
            .spec
            .dependencies
            .iter()
            .find(|d| apps.status(&d.app) != Some(AppStatus::Running));
        let handle = match blocked {
            Some(dep) => failed(&name, format!("dependency {} is not running", dep.app)),
            None => start_app(rt, deployment, spec).await,
        };
        apps.order.push(name.clone());
        apps.apps.insert(name, handle);
    }
    Ok(apps)
}

fn failed(name: &str, why: String) -> AppHandle {
    tracing::error!(app = %name, reason = %why, "app not started");
    AppHandle {
        shared: Arc::new(AppShared {
            name: name.to_string(),
            supervisor: None,
            pool: None,
            status: watch::channel(AppStatus::Failed(ExitReason::Crashed(why))).0,
            stopping: AtomicBool::new(false),
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
        Err(err) => return failed(&name, err.to_string()),
    };
    if pooled {
        for _ in 0..manifest.spec.instances.initial() {
            if let Err(err) = supervisor.start_child().await {
                supervisor.shutdown();
                return failed(&name, err.to_string());
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
            supervisor: Some(supervisor.clone()),
            pool: Some(pool.clone()),
            status,
            stopping: AtomicBool::new(false),
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
    let watcher = handle.clone();
    tokio::spawn(async move {
        let reason = supervisor.wait().await;
        if !watcher.shared.stopping.load(Ordering::Relaxed) {
            tracing::error!(app = %watcher.shared.name, %reason, "app failed");
            watcher
                .shared
                .status
                .send_replace(AppStatus::Failed(reason));
        }
    });
    handle
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
            Some(ChildHandle::Instance(h)) => h.state() == LifecycleState::Active,
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
                    spec: InstanceSpec::from_manifest(manifest),
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
        let mut spec = InstanceSpec::from_manifest(&target.manifest);
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
