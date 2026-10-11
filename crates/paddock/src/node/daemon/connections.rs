//! Connection-handler apps: their listeners, upgrades and rollbacks.

use std::collections::HashMap;
use std::time::Duration;

use crate::api::{ListenerInfo, RolloutParams, UpgradeResponse};
use paddock_bundle::manifest::AppManifest;
use paddock_host::{InstanceSpec, Listener, ListenerSpec, LoadedConnection};

use super::Node;
use super::upgrade::upgrade_error;
use crate::node::error::NodeError;

/// A connection-handler app this node runs: its listeners, and the
/// component they hand new connections to.
pub(super) struct ConnectionApp {
    pub(super) listeners: Vec<Listener>,
    pub(super) current: ConnectionBuild,
    pub(super) previous: Option<ConnectionBuild>,
}

impl std::fmt::Debug for ConnectionApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionApp")
            .field("listeners", &self.listeners)
            .finish_non_exhaustive()
    }
}

/// A compiled connection handler with the spec its instances run with.
#[derive(Clone)]
pub(super) struct ConnectionBuild {
    pub(super) manifest: AppManifest,
    pub(super) connection: LoadedConnection,
    pub(super) spec: InstanceSpec,
}

impl Node {
    pub(super) fn connection_apps(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, ConnectionApp>> {
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Bind every listener a connection handler declares, at the address
    /// node config gives it.
    pub(super) fn listen(&self, build: &ConnectionBuild) -> Result<Vec<Listener>, NodeError> {
        let mut started: Vec<Listener> = Vec::new();
        for decl in &build.manifest.spec.listeners {
            let bound = self.config.listeners.get(&decl.name).ok_or_else(|| {
                NodeError::Invalid(format!(
                    "listener '{}' is not bound in node config ([listeners.{}])",
                    decl.name, decl.name
                ))
            });
            let listener = bound.and_then(|b| {
                let mut spec = ListenerSpec::new(&decl.name, b.address);
                spec.acceptors = b.acceptors;
                spec.max_connections = usize::try_from(decl.max_connections).unwrap_or(usize::MAX);
                spec.idle_timeout = Duration::from_millis(decl.idle_timeout_ms);
                self.rt
                    .listen(spec, build.connection.clone(), build.spec.clone())
                    .map_err(|e| NodeError::Invalid(e.to_string()))
            });
            match listener {
                Ok(l) => started.push(l),
                Err(e) => {
                    for l in &started {
                        l.stop();
                    }
                    return Err(e);
                }
            }
        }
        Ok(started)
    }

    /// Stop a connection handler's listeners, if it is one. Returns whether
    /// it was.
    pub(super) fn stop_connections(&self, name: &str) -> bool {
        let Some(app) = self.connection_apps().remove(name) else {
            return false;
        };
        for l in &app.listeners {
            l.stop();
        }
        true
    }

    /// Upgrade a connection handler: new connections get `build`; open ones
    /// keep their version.
    pub(super) fn upgrade_connections(
        &self,
        name: &str,
        build: ConnectionBuild,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let mut apps = self.connection_apps();
        let app = apps.get_mut(name).ok_or_else(|| {
            NodeError::Invalid(format!(
                "{name} is not a connection handler; the new version's world does not match"
            ))
        })?;
        let approved =
            paddock_host::validate_upgrade(&app.current.manifest, &build.manifest, params.approve)
                .map_err(upgrade_error)?;
        let names = |m: &AppManifest| {
            m.spec
                .listeners
                .iter()
                .map(|l| l.name.clone())
                .collect::<Vec<_>>()
        };
        if names(&app.current.manifest) != names(&build.manifest) {
            return Err(NodeError::Invalid(format!(
                "{name}'s listeners change between versions; remove and deploy it instead"
            )));
        }
        for listener in &app.listeners {
            listener.replace(build.connection.clone(), build.spec.clone());
        }
        let from = app.current.manifest.app.version.to_string();
        let to = build.manifest.app.version.to_string();
        app.previous = Some(std::mem::replace(&mut app.current, build));
        Ok(UpgradeResponse {
            from,
            to,
            mode: "new-connections".into(),
            instances: Vec::new(),
            pauses_us: Vec::new(),
            approved: approved.iter().map(|c| c.key().to_string()).collect(),
        })
    }

    /// Roll a connection handler back to its previous version, for new
    /// connections. `None` if `name` is not a connection handler.
    pub(super) fn rollback_connections(
        &self,
        name: &str,
    ) -> Option<Result<UpgradeResponse, NodeError>> {
        let mut apps = self.connection_apps();
        let app = apps.get_mut(name)?;
        let Some(previous) = app.previous.take() else {
            return Some(Err(NodeError::Conflict(format!(
                "{name} has no previous version"
            ))));
        };
        for listener in &app.listeners {
            listener.replace(previous.connection.clone(), previous.spec.clone());
        }
        let from = app.current.manifest.app.version.to_string();
        let to = previous.manifest.app.version.to_string();
        app.previous = Some(std::mem::replace(&mut app.current, previous));
        Some(Ok(UpgradeResponse {
            from,
            to,
            mode: "new-connections".into(),
            instances: Vec::new(),
            pauses_us: Vec::new(),
            approved: Vec::new(),
        }))
    }
}

pub(super) fn listener_info(l: &Listener) -> ListenerInfo {
    let stats = l.stats();
    ListenerInfo {
        name: l.spec().name.clone(),
        address: l.local_addr().to_string(),
        active: stats.active,
        accepted: stats.accepted,
        refused: stats.refused,
        crashed: stats.crashed,
        idle_closed: stats.idle_closed,
    }
}
