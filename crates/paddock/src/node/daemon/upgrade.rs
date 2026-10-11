//! Upgrades and rollbacks.

use std::path::Path;
use std::time::Duration;

use crate::api::{RolloutParams, UpgradeResponse};
use paddock_bundle::manifest::UpgradeMode;
use paddock_host::{AppDeployment, UpgradeError, UpgradeOptions, UpgradeReport};
use serde_json::json;

use super::Node;
use super::deploy::{Runnable, check_names};
use crate::node::error::{NodeError, internal};

pub(super) fn upgrade_error(e: UpgradeError) -> NodeError {
    match e {
        UpgradeError::UnknownApp(_) => NodeError::NotFound(e.to_string()),
        UpgradeError::InProgress(_) | UpgradeError::NoPrevious(_) => {
            NodeError::Conflict(e.to_string())
        }
        UpgradeError::Invalid(_) => NodeError::Invalid(e.to_string()),
        _ => NodeError::Refused(e.to_string()),
    }
}

/// [`upgrade_error`], with a pointer to `restart` when a snapshot-mode
/// rollout reverted and a restart-mode one might not.
pub(super) fn rollout_error(e: UpgradeError, snapshot: bool, params: &RolloutParams) -> NodeError {
    if snapshot && !params.restart && matches!(e, UpgradeError::Reverted(_)) {
        return NodeError::Refused(format!(
            "{e}; --restart (API: restart=true) moves the instances by restart instead, discarding their in-memory state"
        ));
    }
    upgrade_error(e)
}

pub(super) fn upgrade_response(report: &UpgradeReport) -> UpgradeResponse {
    UpgradeResponse {
        from: report.from.to_string(),
        to: report.to.to_string(),
        mode: format!("{:?}", report.mode).to_lowercase(),
        instances: report
            .instances
            .iter()
            .map(|p| format!("{}.{}.{}", p.node, p.id, p.incarnation))
            .collect(),
        pauses_us: report
            .pauses
            .iter()
            .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
            .collect(),
        approved: report
            .approved
            .iter()
            .map(|c| c.key().to_string())
            .collect(),
    }
}

pub(super) fn options(params: &RolloutParams) -> UpgradeOptions {
    UpgradeOptions {
        approve: params.approve,
        batch_size: params.batch.unwrap_or(1),
        observe: Duration::from_millis(params.observe_ms.unwrap_or(0)),
        max_restarts: params.max_restarts.unwrap_or(0),
        restart: params.restart,
    }
}

impl Node {
    /// Upgrade a running app to bundle bytes.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`], [`NodeError::Conflict`] for a stopped app or
    /// the same version, and [`NodeError::Refused`] for a refused bundle or a
    /// rollout that reverted.
    pub async fn upgrade(
        &self,
        name: &str,
        bytes: &[u8],
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let _change = self.change.lock().await;
        let upload = self.store.write_upload(bytes)?;
        let result = self.upgrade_upload(name, &upload, params).await;
        let _ = std::fs::remove_file(&upload);
        match &result {
            Ok(r) => self.audit.record(
                "upgrade",
                json!({ "app": name, "from": r.from, "to": r.to, "mode": r.mode, "restart_requested": params.restart, "approved": r.approved, "granted": self.st().info.get(name).map(|i| i.granted.clone()) }),
            ),
            Err(e) => self.audit.record("upgrade_refused", json!({ "app": name, "kind": e.kind(), "reason": e.to_string() })),
        }
        result
    }

    async fn upgrade_upload(
        &self,
        name: &str,
        upload: &Path,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let current = self
            .st()
            .desired
            .apps
            .get(name)
            .cloned()
            .ok_or_else(|| NodeError::NotFound(format!("{name} is not deployed")))?;
        if !current.running {
            return Err(NodeError::Conflict(format!(
                "{name} is stopped; start it first"
            )));
        }
        let built = self.build(upload).await?;
        let version = built.version();
        if built.name() != name {
            return Err(NodeError::Invalid(format!(
                "the bundle is {}, not {name}",
                built.name()
            )));
        }
        if version == current.current {
            return Err(NodeError::Conflict(format!(
                "{name} {version} is already running"
            )));
        }
        check_names(&self.st(), name, &built.info.names)?;
        built
            .bundle
            .extract_priv(&self.store.priv_dir(name, &version))
            .map_err(internal)?;
        self.store.keep_upload(upload, name, &version)?;
        let info = built.info.clone();
        let result = match built.run {
            Runnable::Connection(build) => self
                .upgrade_connections(name, build, params)
                .map_err(|e| (e, false)),
            Runnable::App(deployment) if self.held(name) && self.apps.app(name).is_none() => {
                self.upgrade_held(name, deployment, params)
            }
            Runnable::App(deployment) => {
                let snapshot = deployment.manifest.spec.upgrade.mode == UpgradeMode::Snapshot;
                let before = self.ingress.save_routes(name);
                match self.ingress.set_routes(name, &deployment) {
                    Err(e) => Err((e, false)),
                    Ok(()) => {
                        let upgraded = self
                            .apps
                            .upgrade(name, deployment, &options(params))
                            .await
                            .map(|r| upgrade_response(&r))
                            .map_err(|e| (rollout_error(e, snapshot, params), true));
                        if upgraded.is_err() {
                            self.ingress.restore_routes(name, before);
                        }
                        if let Some(gate) = &self.gate {
                            gate.sync(name);
                        }
                        upgraded
                    }
                }
            }
        };
        match result {
            Ok(report) => {
                if let Some(dropped) = current.previous.as_ref().filter(|p| **p != version) {
                    self.store.remove_version(name, dropped);
                }
                {
                    let mut st = self.st();
                    let entry = st
                        .desired
                        .apps
                        .get_mut(name)
                        .ok_or_else(|| internal("app vanished"))?;
                    entry.previous = Some(std::mem::replace(&mut entry.current, version));
                }
                self.save_desired()?;
                let mut st = self.st();
                if let Some(old) = st.info.insert(name.to_string(), info) {
                    st.previous_info.insert(name.to_string(), old);
                }
                Ok(report)
            }
            Err((e, _)) => {
                if current.previous.as_deref() != Some(version.as_str()) {
                    self.store.remove_version(name, &version);
                }
                Err(e)
            }
        }
    }

    /// Upgrade a singleton this node holds but does not run: a promotion
    /// starts the new version.
    fn upgrade_held(
        &self,
        name: &str,
        deployment: AppDeployment,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, (NodeError, bool)> {
        let gate = self
            .gate
            .as_ref()
            .ok_or_else(|| (internal("no failover"), false))?;
        let to = deployment.manifest.app.version.to_string();
        let (from, approved) = gate
            .upgrade_held(name, deployment, params.approve)
            .map_err(|e| (upgrade_error(e), false))?;
        Ok(UpgradeResponse {
            from,
            to,
            mode: "held".into(),
            instances: Vec::new(),
            pauses_us: Vec::new(),
            approved,
        })
    }

    /// Roll back a singleton this node holds but does not run.
    fn rollback_held(&self, name: &str) -> Result<UpgradeResponse, NodeError> {
        let gate = self.gate.as_ref().ok_or_else(|| internal("no failover"))?;
        let (from, to) = gate.rollback_held(name).map_err(upgrade_error)?;
        Ok(UpgradeResponse {
            from,
            to,
            mode: "held".into(),
            instances: Vec::new(),
            pauses_us: Vec::new(),
            approved: Vec::new(),
        })
    }

    /// Roll a running app back to its previous version.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`], [`NodeError::Conflict`] when there is no
    /// previous version, [`NodeError::Refused`] if the rollout reverted.
    pub async fn rollback(
        &self,
        name: &str,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let _change = self.change.lock().await;
        {
            let st = self.st();
            if !st.desired.apps.contains_key(name) {
                return Err(NodeError::NotFound(format!("{name} is not deployed")));
            }
            if let Some(previous) = st.previous_info.get(name) {
                check_names(&st, name, &previous.names)?;
            }
        }
        let result = match self.rollback_connections(name) {
            Some(result) => result,
            None if self.held(name) && self.apps.app(name).is_none() => self.rollback_held(name),
            None => {
                let snapshot = self.apps.app(name).and_then(|a| a.rollback_mode())
                    == Some(UpgradeMode::Snapshot);
                let rolled = self
                    .apps
                    .rollback(name, &options(params))
                    .await
                    .map(|r| upgrade_response(&r))
                    .map_err(|e| rollout_error(e, snapshot, params));
                if rolled.is_ok()
                    && let Some(app) = self.apps.app(name)
                    && let Err(e) = self.ingress.set_routes(name, &app.deployment())
                {
                    tracing::warn!(app = name, error = %e, "routes after rollback");
                }
                if let Some(gate) = &self.gate {
                    gate.sync(name);
                }
                rolled
            }
        };
        match &result {
            Ok(r) => {
                if let Some(entry) = self.st().desired.apps.get_mut(name)
                    && let Some(previous) = entry.previous.take()
                {
                    entry.previous = Some(std::mem::replace(&mut entry.current, previous));
                }
                self.save_desired()?;
                let mut st = self.st();
                let (now, before) = (st.previous_info.remove(name), st.info.remove(name));
                if let Some(i) = now {
                    st.info.insert(name.to_string(), i);
                }
                if let Some(i) = before {
                    st.previous_info.insert(name.to_string(), i);
                }
                drop(st);
                self.audit.record(
                    "rollback",
                    json!({ "app": name, "from": r.from, "to": r.to, "mode": r.mode, "restart_requested": params.restart }),
                );
            }
            Err(e) => self.audit.record(
                "rollback_refused",
                json!({ "app": name, "reason": e.to_string() }),
            ),
        }
        result
    }
}
