//! Reading the node's state, and calling, casting to and killing
//! instances.

use std::time::Duration;

use crate::api::{
    AppDetail, AppSummary, Done, InstanceInfo, LogLine, MonitorInfo, StateResponse, Target,
};
use paddock_host::{AppHandle, AppStatus, CallError, Envelope, InstanceHandle, Pid, Runtime};
use serde_json::json;

use super::Node;
use super::connections::listener_info;
use crate::node::error::NodeError;
use crate::node::logring::LogFeed;
use crate::node::store::AppState;

/// What a call or cast target resolved to.
pub(super) enum Resolved {
    App(AppHandle),
    Instance(InstanceHandle),
    /// A pid on another node, reached through the cluster.
    Remote(Pid),
}

/// The node error for a failed call or cast to `target`.
pub(super) fn send_error(verb: &str, target: &Target, e: &CallError) -> NodeError {
    match e {
        CallError::Noproc => NodeError::NotFound(format!("{target} has no running instance")),
        CallError::TooLarge => NodeError::Invalid(e.to_string()),
        other => NodeError::Conflict(format!("{verb} to {target} failed: {other}")),
    }
}

pub(super) fn status_text(status: &AppStatus) -> (String, Option<String>) {
    match status {
        AppStatus::Starting => ("starting".into(), None),
        AppStatus::Running => ("running".into(), None),
        AppStatus::Failed(reason) => ("failed".into(), Some(reason.to_string())),
        AppStatus::Stopped => ("stopped".into(), None),
    }
}

pub(super) fn instance_info(rt: &Runtime, h: &InstanceHandle) -> InstanceInfo {
    let pid = h.pid();
    let links = rt.links(pid);
    let monitors = |v: &[(u64, Pid)]| {
        v.iter()
            .map(|(reference, pid)| MonitorInfo {
                reference: *reference,
                pid: pid.text(),
            })
            .collect()
    };
    InstanceInfo {
        pid: pid.text(),
        app: h.app().to_string(),
        version: h.version(),
        state: format!("{:?}", h.state()).to_lowercase(),
        queued: h.queued(),
        dropped: h.dropped_messages(),
        memory_bytes: h.memory_bytes(),
        restarts: h.restarts(),
        last_exit: h.last_exit().map(|r| r.to_string()),
        links: links.links.iter().map(|p| p.text()).collect(),
        monitoring: monitors(&links.monitoring),
        monitored_by: monitors(&links.monitored_by),
    }
}

impl Node {
    pub(super) fn summary(
        &self,
        name: &str,
        app: &AppState,
        restore_error: Option<&String>,
    ) -> AppSummary {
        let handle = self.apps.app(name);
        // A connection handler's open connections, or an HTTP handler's
        // requests in flight: its instances now.
        let connections = self
            .connection_apps()
            .get(name)
            .map(|c| c.listeners.iter().map(|l| l.stats().active).sum::<usize>())
            .or_else(|| {
                self.handler_apps()
                    .contains_key(name)
                    .then(|| self.ingress.handler_requests(name).unwrap_or(0))
            });
        let (status, reason) = match (&handle, connections, restore_error) {
            (Some(h), _, _) => status_text(&h.status()),
            (None, Some(_), _) => ("running".into(), None),
            (None, None, _) if self.held(name) => (
                "standby".into(),
                Some("a singleton; runs while this node is primary".into()),
            ),
            (None, None, Some(e)) => ("failed".into(), Some(e.clone())),
            (None, None, None) => ("stopped".into(), None),
        };
        AppSummary {
            name: name.to_string(),
            version: app.current.clone(),
            status,
            reason,
            instances: handle.map_or(connections.unwrap_or(0), |h| h.instances().len()),
            previous: app.previous.clone(),
            new_version: app.new_side.as_ref().map(|n| n.version.clone()),
            new_traffic: app.new_side.as_ref().map(|n| n.percent),
        }
    }

    /// Kill an instance; its supervisor restarts it.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] for a malformed pid, [`NodeError::NotFound`]
    /// for no live instance with it.
    pub async fn kill(&self, pid: &str) -> Result<Done, NodeError> {
        let pid: Pid = pid.parse().map_err(NodeError::Invalid)?;
        let handle = self
            .rt
            .lookup(pid)
            .ok_or_else(|| NodeError::NotFound(format!("no instance {pid}")))?;
        handle.kill();
        self.audit.record(
            "kill",
            json!({ "pid": pid.to_string(), "app": handle.app() }),
        );
        Ok(Done {
            message: format!("killed {pid}"),
        })
    }

    /// Every deployed app. Does not wait for a change in progress: an app
    /// being deployed appears once its deploy is done.
    #[must_use]
    pub fn list(&self) -> Vec<AppSummary> {
        let apps: Vec<(String, AppState, Option<String>)> = {
            let st = self.st();
            st.desired
                .order
                .iter()
                .filter_map(|n| {
                    let app = st.desired.apps.get(n)?.clone();
                    Some((n.clone(), app, st.restore_errors.get(n).cloned()))
                })
                .collect()
        };
        apps.iter()
            .map(|(n, a, e)| self.summary(n, a, e.as_ref()))
            .collect()
    }

    /// One app in detail.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`].
    pub fn status(&self, name: &str) -> Result<AppDetail, NodeError> {
        let (app, info, restore_error) = {
            let st = self.st();
            let app = st
                .desired
                .apps
                .get(name)
                .cloned()
                .ok_or_else(|| NodeError::NotFound(format!("{name} is not deployed")))?;
            let info = st.info.get(name).cloned().unwrap_or_default();
            (app, info, st.restore_errors.get(name).cloned())
        };
        let handle = self.apps.app(name);
        Ok(AppDetail {
            summary: self.summary(name, &app, restore_error.as_ref()),
            signer: info.signer,
            granted: info.granted,
            denied: info.denied,
            stubbed: info.stubbed,
            warnings: info.warnings,
            restarts: handle
                .as_ref()
                .and_then(AppHandle::supervisor)
                .map_or(0, |s| s.restart_count()),
            instance_list: handle
                .map(|h| {
                    h.instances()
                        .iter()
                        .map(|i| instance_info(&self.rt, i))
                        .collect()
                })
                .unwrap_or_default(),
            listeners: self
                .connection_apps()
                .get(name)
                .map(|c| c.listeners.iter().map(listener_info).collect())
                .unwrap_or_default(),
        })
    }

    /// Every live instance.
    #[must_use]
    pub fn ps(&self) -> Vec<InstanceInfo> {
        self.rt
            .instances()
            .iter()
            .map(|i| instance_info(&self.rt, i))
            .collect()
    }

    /// An app's last `lines` log records.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`].
    pub fn logs(&self, name: &str, lines: usize) -> Result<Vec<LogLine>, NodeError> {
        if !self.st().desired.apps.contains_key(name) {
            return Err(NodeError::NotFound(format!("{name} is not deployed")));
        }
        Ok(self.logs.recent(name, lines))
    }

    /// `name`'s last `lines` log records, and a feed of every app's new
    /// records (filter by app name).
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] for an app that is not deployed.
    pub(crate) fn follow_logs(
        &self,
        name: &str,
        lines: usize,
    ) -> Result<(Vec<LogLine>, LogFeed), NodeError> {
        if !self.st().desired.apps.contains_key(name) {
            return Err(NodeError::NotFound(format!("{name} is not deployed")));
        }
        Ok(self.logs.follow(name, lines))
    }

    /// The node's events from now on, each a line of JSON.
    pub(crate) fn follow_events(&self) -> tokio::sync::broadcast::Receiver<std::sync::Arc<str>> {
        self.events.subscribe()
    }

    /// Turns true when the node shuts down.
    pub(crate) fn stopping(&self) -> tokio::sync::watch::Receiver<bool> {
        self.stop.subscribe()
    }

    /// Call an app, routed to one of its instances.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] for an app that is not running,
    /// [`NodeError::Invalid`] for a body over its `message_max_bytes`, and
    /// [`NodeError::Conflict`] for the other call failures.
    pub async fn call(
        &self,
        name: &str,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, NodeError> {
        self.call_to(&Target::App(name.to_string()), msg, timeout)
            .await
    }

    /// Call an app (through its pool), an instance by pid, or the instance
    /// holding a registered name.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] for an app that is not running, an unknown
    /// pid or an unregistered name, [`NodeError::Invalid`] for a bad pid or
    /// a body over `message_max_bytes`, and [`NodeError::Conflict`] for the
    /// other call failures.
    pub async fn call_to(
        &self,
        target: &Target,
        msg: Envelope,
        timeout: Duration,
    ) -> Result<Envelope, NodeError> {
        let result = match self.resolve(target)? {
            Resolved::App(app) => app.call(msg, timeout).await,
            Resolved::Instance(h) => h.call(msg, timeout).await,
            Resolved::Remote(pid) => self.rt.call(pid, msg, timeout).await,
        };
        result.map_err(|e| send_error("call", target, &e))
    }

    /// Cast to an app, an instance by pid, or a registered name.
    ///
    /// # Errors
    ///
    /// As for [`Node::call_to`]; a full mailbox is [`NodeError::Conflict`]
    /// (the cast is dropped and counted).
    pub fn cast_to(&self, target: &Target, msg: Envelope) -> Result<Done, NodeError> {
        match self.resolve(target)? {
            Resolved::App(app) => app.cast(msg),
            Resolved::Instance(h) => h.cast(msg),
            Resolved::Remote(pid) => self.rt.cast(pid, msg),
        }
        .map_err(|e| send_error("cast", target, &e))?;
        Ok(Done {
            message: format!("cast to {target}"),
        })
    }

    fn resolve(&self, target: &Target) -> Result<Resolved, NodeError> {
        match target {
            Target::App(name) => self
                .apps
                .app(name)
                .map(Resolved::App)
                .ok_or_else(|| NodeError::NotFound(format!("{name} is not running"))),
            Target::Pid(pid) => {
                let pid: Pid = pid.parse().map_err(NodeError::Invalid)?;
                if pid.node != self.rt.node() {
                    return Ok(Resolved::Remote(pid));
                }
                self.rt
                    .lookup(pid)
                    .map(Resolved::Instance)
                    .ok_or_else(|| NodeError::NotFound(format!("no instance {pid}")))
            }
            Target::Name(name) => self
                .rt
                .whereis(name)
                .map(Resolved::Instance)
                .ok_or_else(|| NodeError::NotFound(format!("no instance registered as {name}"))),
        }
    }

    /// Cast to an app, routed to one of its instances.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] for an app that is not running,
    /// [`NodeError::Invalid`] for a body over its `message_max_bytes`, and
    /// [`NodeError::Conflict`] for a full mailbox (the cast is dropped and
    /// counted).
    pub fn cast(&self, name: &str, msg: Envelope) -> Result<Done, NodeError> {
        self.cast_to(&Target::App(name.to_string()), msg)
    }

    /// An instance's state through `save-snapshot`.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] for a malformed pid, [`NodeError::NotFound`]
    /// for no live instance, and [`NodeError::Conflict`] if `save-snapshot`
    /// fails (the instance then restarts).
    pub async fn state(&self, pid: &str) -> Result<StateResponse, NodeError> {
        let pid: Pid = pid.parse().map_err(NodeError::Invalid)?;
        let handle = self
            .rt
            .lookup(pid)
            .ok_or_else(|| NodeError::NotFound(format!("no instance {pid}")))?;
        let bytes = handle
            .snapshot()
            .await
            .map_err(|e| NodeError::Conflict(format!("{pid}: {e}")))?;
        let shown = pid.text();
        Ok(super::super::state_view::describe(&shown, &bytes))
    }

    /// Set every instance of an app to log at `level` (`trace` to
    /// `critical`), or with `reset`, at its own level again.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] for an app not deployed, [`NodeError::Invalid`]
    /// for an unknown level.
    pub fn set_log_level(&self, name: &str, level: &str) -> Result<Done, NodeError> {
        if !self.st().desired.apps.contains_key(name) {
            return Err(NodeError::NotFound(format!("{name} is not deployed")));
        }
        let parsed = if level == "reset" {
            None
        } else {
            Some(
                serde_json::from_value::<paddock_bundle::manifest::LogLevel>(json!(level)).map_err(|_| {
                    NodeError::Invalid(format!(
                        "unknown log level '{level}' (trace, debug, info, warn, error, critical, reset)"
                    ))
                })?,
            )
        };
        self.rt.set_log_level(name, parsed);
        self.audit
            .record("log_level", json!({ "app": name, "level": level }));
        let message = if parsed.is_some() {
            format!("{name} logs at {level}")
        } else {
            format!("{name} logs at its manifest level")
        };
        Ok(Done { message })
    }
}
