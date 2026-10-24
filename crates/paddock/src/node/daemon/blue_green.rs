//! Blue-green upgrades (DESIGN.md, "Blue-green upgrades"): the
//! new version beside the old, traffic moved in steps, then the old version
//! retired or the new one dropped. The host keeps both sides in the app's
//! pool ([`paddock_host::Apps::start_new`]); this keeps the deploy store in
//! step with them, so a restart brings back both at the same weight.

use std::time::Duration;

use paddock_host::{AppDeployment, UpgradeReport};
use serde_json::json;

use super::Node;
use super::upgrade::{upgrade_error, upgrade_response};
use super::{DeployInfo, ingress};
use crate::api::{RolloutParams, UpgradeResponse};
use crate::node::error::{NodeError, internal};
use crate::node::gate;
use crate::node::store::NewSideState;

/// How long the side that stops may take to empty its mailboxes, unless
/// asked otherwise.
const DRAIN: Duration = Duration::from_secs(10);

/// A change to a blue-green upgrade under way (`paddock traffic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Traffic {
    /// Give the new version this share of traffic, in percent.
    Percent(u8),
    /// Retire the old version.
    Promote,
    /// Stop the new version.
    Drop,
}

fn drain(params: &RolloutParams) -> Duration {
    params.drain_ms.map_or(DRAIN, Duration::from_millis)
}

impl Node {
    /// Upgrade `name` blue-green to `deployment` (already stored as
    /// `version`): start it beside the running version, move traffic by
    /// `params.steps`, then promote it if the last step is 100. A step whose
    /// watch sees the new version give up, or restart more than allowed,
    /// drops it, and the old version serves alone again.
    pub(super) async fn upgrade_blue_green(
        &self,
        name: &str,
        deployment: AppDeployment,
        info: DeployInfo,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let version = deployment.manifest.app.version.to_string();
        if gate::is_singleton(&deployment) || self.held(name) {
            return Err(NodeError::Invalid(format!(
                "{name} is a singleton, which runs one instance: upgrade it in snapshot or restart mode"
            )));
        }
        let app = self
            .apps
            .app(name)
            .ok_or_else(|| NodeError::Conflict(format!("{name} is not running")))?;
        if !ingress::same_routes(&app.deployment().manifest, &deployment.manifest) {
            return Err(NodeError::Invalid(format!(
                "{name} {version} changes its HTTP routes, which both versions would serve at once"
            )));
        }
        let steps = params.steps.clone().unwrap_or_else(|| vec![100]);
        let approved = self
            .apps
            .start_new(name, deployment, params.approve)
            .await
            .map_err(upgrade_error)?;
        self.set_new_side(name, Some(&version), 0)?;
        self.st().new_info.insert(name.to_string(), info);
        let base = app.new_side().map_or(0, |s| s.restarts);
        let observe = Duration::from_millis(params.observe_ms.unwrap_or(0));
        let max_restarts = params.max_restarts.unwrap_or(0);
        for &percent in &steps {
            if let Err(e) = self.apps.set_traffic(name, percent) {
                return Err(self.abandon(name, params, &e.to_string()).await);
            }
            self.set_new_side(name, Some(&version), percent)?;
            tokio::time::sleep(observe).await;
            let why = match app.new_side() {
                None => Some(format!(
                    "the new version gave up at {percent}%: {}",
                    app.new_side_failure().unwrap_or_default()
                )),
                Some(s) if s.restarts.saturating_sub(base) > max_restarts => Some(format!(
                    "{} restarts of the new version at {percent}% (limit {max_restarts})",
                    s.restarts.saturating_sub(base)
                )),
                Some(_) => None,
            };
            if let Some(why) = why {
                return Err(self.abandon(name, params, &why).await);
            }
        }
        if steps.last() == Some(&100) {
            return self.finish_promote(name, params).await;
        }
        let report = UpgradeReport {
            from: app.version(),
            to: version.parse().map_err(internal)?,
            mode: paddock_bundle::manifest::UpgradeMode::BlueGreen,
            instances: Vec::new(),
            pauses: Vec::new(),
            approved,
        };
        Ok(UpgradeResponse {
            traffic: Some(steps.last().copied().unwrap_or(0)),
            ..upgrade_response(&report)
        })
    }

    /// Change a blue-green upgrade under way.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] for an app not deployed, [`NodeError::Invalid`]
    /// if no new version runs beside it.
    pub async fn traffic(
        &self,
        name: &str,
        change: Traffic,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let _change = self.change.lock().await;
        let new = self
            .st()
            .desired
            .apps
            .get(name)
            .ok_or_else(|| NodeError::NotFound(format!("{name} is not deployed")))?
            .new_side
            .clone()
            .ok_or_else(|| {
                NodeError::Invalid(format!("{name} has no new version running beside it"))
            })?;
        let result = match change {
            Traffic::Percent(percent) => {
                self.apps
                    .set_traffic(name, percent)
                    .map_err(upgrade_error)?;
                self.set_new_side(name, Some(&new.version), percent.min(100))?;
                let current = self.st().desired.apps.get(name).map(|a| a.current.clone());
                Ok(UpgradeResponse {
                    from: current.unwrap_or_default(),
                    to: new.version.clone(),
                    mode: "blue-green".into(),
                    instances: Vec::new(),
                    pauses_us: Vec::new(),
                    approved: Vec::new(),
                    traffic: Some(percent.min(100)),
                })
            }
            Traffic::Promote => self.finish_promote(name, params).await,
            Traffic::Drop => self.drop_new_side(name, params).await,
        };
        match &result {
            Ok(r) => self.audit.record(
                "traffic",
                json!({ "app": name, "change": format!("{change:?}").to_lowercase(), "to": r.to, "traffic": r.traffic }),
            ),
            Err(e) => self.audit.record(
                "traffic_refused",
                json!({ "app": name, "kind": e.kind(), "reason": e.to_string() }),
            ),
        }
        result
    }

    /// Retire the old side: the new version becomes current, the old one
    /// previous, and the version before that is dropped from the store.
    async fn finish_promote(
        &self,
        name: &str,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let report = self
            .apps
            .promote(name, drain(params))
            .await
            .map_err(upgrade_error)?;
        let dropped = {
            let mut st = self.st();
            let entry = st
                .desired
                .apps
                .get_mut(name)
                .ok_or_else(|| internal("app vanished"))?;
            let new = entry
                .new_side
                .take()
                .ok_or_else(|| internal("no new side recorded"))?;
            let dropped = entry.previous.take();
            entry.previous = Some(std::mem::replace(&mut entry.current, new.version));
            dropped
        };
        self.save_desired()?;
        if let Some(dropped) = dropped {
            self.store.remove_version(name, &dropped);
        }
        let mut st = self.st();
        if let Some(info) = st.new_info.remove(name)
            && let Some(old) = st.info.insert(name.to_string(), info)
        {
            st.previous_info.insert(name.to_string(), old);
        }
        Ok(upgrade_response(&report))
    }

    /// Stop the new side and forget it.
    pub(super) async fn drop_new_side(
        &self,
        name: &str,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let version = self
            .apps
            .drop_new(name, drain(params))
            .await
            .map_err(upgrade_error)?
            .to_string();
        self.forget_new_side(name, &version)?;
        let current = self.st().desired.apps.get(name).map(|a| a.current.clone());
        Ok(UpgradeResponse {
            from: version,
            to: current.unwrap_or_default(),
            mode: "blue-green".into(),
            instances: Vec::new(),
            pauses_us: Vec::new(),
            approved: Vec::new(),
            traffic: None,
        })
    }

    /// A step failed: drop the new side (if it is still there) and say why.
    async fn abandon(&self, name: &str, params: &RolloutParams, why: &str) -> NodeError {
        let version = self
            .st()
            .desired
            .apps
            .get(name)
            .and_then(|a| a.new_side.as_ref().map(|n| n.version.clone()));
        tracing::warn!(app = %name, reason = %why, "blue-green upgrade stopped; dropping the new version");
        if self.apps.app(name).and_then(|a| a.new_side()).is_some()
            && let Err(e) = self.apps.drop_new(name, drain(params)).await
        {
            tracing::error!(app = %name, error = %e, "the new version could not be dropped");
        }
        if let Some(version) = version
            && let Err(e) = self.forget_new_side(name, &version)
        {
            return e;
        }
        NodeError::Refused(format!(
            "blue-green upgrade stopped: {why}; the new version was dropped and the old one serves alone"
        ))
    }

    fn forget_new_side(&self, name: &str, version: &str) -> Result<(), NodeError> {
        let previous = self.set_new_side(name, None, 0)?;
        self.st().new_info.remove(name);
        if previous.as_deref() != Some(version) {
            self.store.remove_version(name, version);
        }
        Ok(())
    }

    /// Record the new side's version and share (`None` to forget it), and
    /// return the app's previous version.
    fn set_new_side(
        &self,
        name: &str,
        version: Option<&str>,
        percent: u8,
    ) -> Result<Option<String>, NodeError> {
        let previous = {
            let mut st = self.st();
            let entry = st
                .desired
                .apps
                .get_mut(name)
                .ok_or_else(|| internal("app vanished"))?;
            entry.new_side = version.map(|v| NewSideState {
                version: v.to_string(),
                percent,
            });
            entry.previous.clone()
        };
        self.save_desired()?;
        Ok(previous)
    }

    /// After a restart: start the new side recorded for `name` again, at
    /// its share of traffic.
    pub(super) async fn restore_new_side(
        &self,
        name: &str,
        new: &NewSideState,
    ) -> Result<(), NodeError> {
        let built = self
            .build(&self.store.bundle_path(name, &new.version))
            .await?;
        built
            .bundle
            .extract_priv(&self.store.priv_dir(name, &new.version))
            .map_err(internal)?;
        let super::deploy::Runnable::App(deployment) = built.run else {
            return Err(internal("the new version is not an app"));
        };
        // Approved when it was first started.
        self.apps
            .start_new(name, deployment, true)
            .await
            .map_err(upgrade_error)?;
        self.apps
            .set_traffic(name, new.percent)
            .map_err(upgrade_error)?;
        self.st().new_info.insert(name.to_string(), built.info);
        Ok(())
    }
}
