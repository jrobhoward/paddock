//! HTTP-handler apps (`wasi:http/proxy`): an instance per request on their
//! routes, served by the node's HTTP ingress. Upgrades and rollbacks apply
//! to new requests.

// Without the `http` feature no handler is ever compiled (`Compiled` is
// uninhabited), so the code past one is unreachable there.
#![cfg_attr(
    not(feature = "http"),
    allow(unreachable_code, unused_variables, dead_code)
)]

use std::collections::HashMap;

use crate::api::{RolloutParams, UpgradeResponse};
use paddock_bundle::manifest::AppManifest;
use paddock_host::InstanceSpec;

use super::Node;
use super::upgrade::upgrade_error;
use crate::node::error::NodeError;

/// A compiled HTTP handler. A build without the `http` feature has no
/// ingress and never compiles one.
#[cfg(feature = "http")]
pub(super) type Compiled = paddock_host::http::LoadedHandler;

/// A compiled HTTP handler. A build without the `http` feature has no
/// ingress and never compiles one.
#[cfg(not(feature = "http"))]
#[derive(Debug, Clone)]
pub(super) enum Compiled {}

#[cfg(not(feature = "http"))]
impl Compiled {
    pub(super) fn stubbed_imports(&self) -> &[String] {
        match *self {}
    }
}

/// An HTTP handler with the spec its instances run with.
#[derive(Debug, Clone)]
pub(super) struct HandlerBuild {
    pub(super) manifest: AppManifest,
    pub(super) handler: Compiled,
    pub(super) spec: InstanceSpec,
}

/// An HTTP-handler app this node serves.
#[derive(Debug)]
pub(super) struct HandlerApp {
    pub(super) current: HandlerBuild,
    pub(super) previous: Option<HandlerBuild>,
}

impl Node {
    pub(super) fn handler_apps(&self) -> std::sync::MutexGuard<'_, HashMap<String, HandlerApp>> {
        self.handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Start serving `name`'s routes with `build`.
    pub(super) fn serve_handler(&self, name: &str, build: HandlerBuild) -> Result<(), NodeError> {
        self.ingress.set_handler(name, &build)?;
        self.handler_apps().insert(
            name.to_string(),
            HandlerApp {
                current: build,
                previous: None,
            },
        );
        Ok(())
    }

    /// Stop serving an HTTP handler's routes, if it is one; requests in
    /// flight finish. Returns whether it was.
    pub(super) fn stop_handler(&self, name: &str) -> bool {
        if self.handler_apps().remove(name).is_none() {
            return false;
        }
        self.ingress.remove_handler(name);
        true
    }

    /// Upgrade an HTTP handler: new requests get `build`, with its routes.
    pub(super) fn upgrade_handler(
        &self,
        name: &str,
        build: HandlerBuild,
        params: &RolloutParams,
    ) -> Result<UpgradeResponse, NodeError> {
        let mut apps = self.handler_apps();
        let app = apps.get_mut(name).ok_or_else(|| {
            NodeError::Invalid(format!(
                "{name} is not an HTTP handler; the new version's world does not match"
            ))
        })?;
        let approved =
            paddock_host::validate_upgrade(&app.current.manifest, &build.manifest, params.approve)
                .map_err(upgrade_error)?;
        self.ingress.set_handler(name, &build)?;
        let response = new_requests(&app.current, &build);
        app.previous = Some(std::mem::replace(&mut app.current, build));
        Ok(UpgradeResponse {
            approved: approved.iter().map(|c| c.key().to_string()).collect(),
            traffic: None,
            ..response
        })
    }

    /// Roll an HTTP handler back to its previous version, for new requests.
    /// `None` if `name` is not an HTTP handler.
    pub(super) fn rollback_handler(
        &self,
        name: &str,
    ) -> Option<Result<UpgradeResponse, NodeError>> {
        let mut apps = self.handler_apps();
        let app = apps.get_mut(name)?;
        let Some(previous) = app.previous.take() else {
            return Some(Err(NodeError::Conflict(format!(
                "{name} has no previous version"
            ))));
        };
        if let Err(e) = self.ingress.set_handler(name, &previous) {
            app.previous = Some(previous);
            return Some(Err(e));
        }
        let response = new_requests(&app.current, &previous);
        app.previous = Some(std::mem::replace(&mut app.current, previous));
        Some(Ok(response))
    }
}

fn new_requests(from: &HandlerBuild, to: &HandlerBuild) -> UpgradeResponse {
    UpgradeResponse {
        from: from.manifest.app.version.to_string(),
        to: to.manifest.app.version.to_string(),
        mode: "new-requests".into(),
        instances: Vec::new(),
        pauses_us: Vec::new(),
        approved: Vec::new(),
        traffic: None,
    }
}
