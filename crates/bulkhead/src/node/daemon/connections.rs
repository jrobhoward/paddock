//! Native apps' listeners, which hand connections to the app's instances.

use std::collections::HashMap;
use std::time::Duration;

use crate::api::ListenerInfo;
use bulkhead_bundle::manifest::AppManifest;
use bulkhead_host::{AppHandle, Listener, ListenerSpec};

use super::Node;
use crate::node::error::NodeError;

impl Node {
    /// Bind every listener a native app declares, handing connections to
    /// `app`'s instances, and keep them until the app stops.
    pub(super) fn listen_native(
        &self,
        manifest: &AppManifest,
        app: &AppHandle,
    ) -> Result<(), NodeError> {
        if manifest.spec.listeners.is_empty() {
            return Ok(());
        }
        let listeners = self.bind_listeners(manifest, app)?;
        self.native_listener_map()
            .insert(manifest.app.name.clone(), listeners);
        Ok(())
    }

    /// Bind each of `manifest`'s listeners for `app`, at the address node
    /// config gives it; on a failure, stop those already bound.
    fn bind_listeners(
        &self,
        manifest: &AppManifest,
        app: &AppHandle,
    ) -> Result<Vec<Listener>, NodeError> {
        let mut started: Vec<Listener> = Vec::new();
        for decl in &manifest.spec.listeners {
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
                    .listen_native(spec, app.clone())
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

    pub(super) fn native_listener_map(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Listener>>> {
        self.native_listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Stop a native app's listeners, if it has any; their connections
    /// close.
    pub(super) fn stop_native_listeners(&self, name: &str) {
        for l in self.native_listener_map().remove(name).unwrap_or_default() {
            l.stop();
        }
    }

    /// An app's listeners' counters.
    pub(super) fn listener_infos(&self, name: &str) -> Vec<ListenerInfo> {
        self.native_listener_map()
            .get(name)
            .map(|ls| ls.iter().map(listener_info).collect())
            .unwrap_or_default()
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
        idle_closed: stats.idle_closed,
    }
}
