//! The node's HTTP ingress (`[http.<name>]`), its HTTPS certificates and
//! the routes apps declare. Without the `http` feature a node has none, and
//! refuses node config or apps that need one; without `tls`, plain HTTP
//! only.

#[cfg(feature = "http")]
use std::time::Duration;

#[cfg(feature = "http")]
use paddock_bundle::manifest::AppManifest;
#[cfg(feature = "http")]
use paddock_host::http::{HttpIngress, HttpRoute, IngressListen, Standby};
#[cfg(feature = "tls")]
use paddock_host::http::{IngressTls, TlsCertificate};
use paddock_host::{AppDeployment, Apps, InstanceSpec};
use tokio::sync::watch;

use super::handlers::HandlerBuild;
use crate::node::config::NodeConfig;
#[cfg(feature = "http")]
use crate::node::config::StandbyMode;
use crate::node::error::NodeError;

/// The node's ingress, if node config has `[http]` addresses.
#[derive(Debug, Clone, Default)]
pub(super) struct Ingress {
    #[cfg(feature = "http")]
    http: Option<HttpIngress>,
    /// HTTPS certificates, by ingress name.
    #[cfg(feature = "tls")]
    certificates: Vec<(String, IngressTls)>,
}

/// An app's routes, kept to put back after a failed upgrade.
pub(super) struct SavedRoutes(#[cfg(feature = "http")] Option<Vec<HttpRoute>>);

impl Ingress {
    /// Bind every `[http.<name>]` address, and watch certificates that
    /// reload on a timer until `stop`.
    #[cfg(feature = "http")]
    pub(super) async fn open(
        config: &NodeConfig,
        apps: &Apps,
        stop: &watch::Sender<bool>,
    ) -> Result<Self, NodeError> {
        #[cfg(not(feature = "tls"))]
        let _ = stop;
        let mut ingress = Self::default();
        if config.http.is_empty() {
            return Ok(ingress);
        }
        let http = HttpIngress::new(apps.clone());
        for (name, binding) in &config.http {
            let mut listen = IngressListen::new(binding.address);
            listen.acceptors = binding.acceptors;
            listen.max_connections = binding.max_connections;
            listen.header_timeout = Duration::from_millis(binding.header_timeout_ms.max(1));
            listen.body_timeout = Duration::from_millis(binding.body_timeout_ms.max(1));
            listen.max_response = binding.max_response.bytes();
            listen.standby = match (binding.standby_mode(), &binding.partner_url) {
                (StandbyMode::Redirect, Some(url)) => {
                    Standby::Redirect(url.trim_end_matches('/').to_string())
                }
                (StandbyMode::Forward, _) => Standby::Forward,
                // Config checks refuse a redirect without a URL.
                _ => Standby::Refuse,
            };
            if !binding.tls.is_empty() {
                #[cfg(feature = "tls")]
                {
                    let entries = binding
                        .tls
                        .iter()
                        .map(|t| TlsCertificate {
                            cert: t.cert.clone(),
                            key: t.key.clone(),
                            hosts: t.hosts.clone(),
                        })
                        .collect();
                    let tls = IngressTls::load(entries)
                        .map_err(|e| NodeError::Invalid(format!("http.{name}: {e}")))?;
                    if binding.tls_reload_s > 0 {
                        tokio::spawn(watch_certificates(
                            name.clone(),
                            tls.clone(),
                            Duration::from_secs(binding.tls_reload_s),
                            stop.subscribe(),
                        ));
                    }
                    ingress.certificates.push((name.clone(), tls.clone()));
                    listen.tls = Some(tls);
                }
                #[cfg(not(feature = "tls"))]
                return Err(NodeError::Invalid(format!(
                    "http.{name}: this build of paddock has no TLS (cargo feature `tls`)"
                )));
            }
            let bound = http
                .listen(listen)
                .await
                .map_err(|e| NodeError::Invalid(format!("http.{name}: {e}")))?;
            tracing::info!(name, %bound, "HTTP ingress listening");
        }
        ingress.http = Some(http);
        Ok(ingress)
    }

    /// No ingress in this build: refuse node config that asks for one.
    #[cfg(not(feature = "http"))]
    #[allow(clippy::unused_async)] // The same signature as with `http`.
    pub(super) async fn open(
        config: &NodeConfig,
        _apps: &Apps,
        _stop: &watch::Sender<bool>,
    ) -> Result<Self, NodeError> {
        match config.http.keys().next() {
            Some(name) => Err(NodeError::Invalid(format!(
                "http.{name}: this build of paddock has no HTTP ingress (cargo feature `http`)"
            ))),
            None => Ok(Self::default()),
        }
    }

    /// Tell the ingress which apps run on the failover partner now.
    #[cfg(feature = "http")]
    pub(super) fn set_elsewhere(&self, elsewhere: paddock_host::http::Elsewhere) {
        if let Some(http) = &self.http {
            http.set_elsewhere(elsewhere);
        }
    }

    /// Give `name` the HTTP routes `deployment` declares, if the operator
    /// granted them (none otherwise).
    #[cfg(feature = "http")]
    pub(super) fn set_routes(
        &self,
        name: &str,
        deployment: &AppDeployment,
    ) -> Result<(), NodeError> {
        self.set_routes_of(name, &deployment.manifest, &deployment.spec)
    }

    #[cfg(feature = "http")]
    fn set_routes_of(
        &self,
        name: &str,
        manifest: &AppManifest,
        spec: &InstanceSpec,
    ) -> Result<(), NodeError> {
        let routes = if granted_routes(spec) {
            HttpRoute::from_manifest(manifest)
        } else {
            Vec::new()
        };
        match &self.http {
            Some(ingress) => ingress
                .set_routes(name, routes)
                .map_err(|e| NodeError::Conflict(e.to_string())),
            None if routes.is_empty() => Ok(()),
            None => Err(NodeError::Invalid(format!(
                "{name} declares HTTP routes, and this node has no HTTP ingress ([http.<name>] in node config)"
            ))),
        }
    }

    /// Serve `name`'s routes with an instance of `build`'s HTTP handler per
    /// request: its routes (if granted), then its handler. A failure leaves
    /// what was served before.
    #[cfg(feature = "http")]
    pub(super) fn set_handler(&self, name: &str, build: &HandlerBuild) -> Result<(), NodeError> {
        let Some(http) = &self.http else {
            return Err(NodeError::Invalid(format!(
                "{name} is an HTTP handler, and this node has no HTTP ingress ([http.<name>] in node config)"
            )));
        };
        self.set_routes_of(name, &build.manifest, &build.spec)?;
        http.set_handler(name, build.handler.clone(), build.spec.clone());
        Ok(())
    }

    /// No ingress in this build: no handler was ever compiled.
    #[cfg(not(feature = "http"))]
    pub(super) fn set_handler(&self, _name: &str, build: &HandlerBuild) -> Result<(), NodeError> {
        match build.handler {}
    }

    /// Stop serving `name`'s HTTP handler and withdraw its routes; requests
    /// in flight finish.
    pub(super) fn remove_handler(&self, name: &str) {
        #[cfg(feature = "http")]
        if let Some(http) = &self.http {
            http.remove_handler(name);
            http.remove_routes(name);
        }
        #[cfg(not(feature = "http"))]
        let _ = name;
    }

    /// Requests `name`'s HTTP handler is serving now.
    pub(super) fn handler_requests(&self, name: &str) -> Option<usize> {
        #[cfg(feature = "http")]
        return self.http.as_ref().and_then(|h| h.handler_requests(name));
        #[cfg(not(feature = "http"))]
        {
            let _ = name;
            None
        }
    }

    /// No ingress in this build: refuse an app granted routes.
    #[cfg(not(feature = "http"))]
    pub(super) fn set_routes(
        &self,
        name: &str,
        deployment: &AppDeployment,
    ) -> Result<(), NodeError> {
        let declares = deployment
            .manifest
            .spec
            .capabilities
            .http_incoming
            .as_ref()
            .is_some_and(|scope| !scope.routes.is_empty());
        if granted_routes(&deployment.spec) && declares {
            return Err(NodeError::Invalid(format!(
                "{name} declares HTTP routes, and this build of paddock has no HTTP ingress (cargo feature `http`)"
            )));
        }
        Ok(())
    }

    pub(super) fn remove_routes(&self, name: &str) {
        #[cfg(feature = "http")]
        if let Some(ingress) = &self.http {
            ingress.remove_routes(name);
        }
        #[cfg(not(feature = "http"))]
        let _ = name;
    }

    /// `name`'s routes now, to put back with [`Ingress::restore_routes`].
    pub(super) fn save_routes(&self, name: &str) -> SavedRoutes {
        #[cfg(feature = "http")]
        return SavedRoutes(self.http.as_ref().map(|h| h.routes(name)));
        #[cfg(not(feature = "http"))]
        {
            let _ = name;
            SavedRoutes()
        }
    }

    /// Put back routes saved before a failed upgrade: the old version keeps
    /// running with them. A failure is logged and returned, to add to the
    /// upgrade's error.
    pub(super) fn restore_routes(&self, name: &str, saved: SavedRoutes) -> Result<(), String> {
        #[cfg(feature = "http")]
        if let (Some(http), SavedRoutes(Some(before))) = (&self.http, saved)
            && let Err(e) = http.set_routes(name, before)
        {
            tracing::error!(app = name, error = %e, "HTTP routes not put back after a failed upgrade");
            return Err(format!("its HTTP routes could not be put back: {e}"));
        }
        #[cfg(not(feature = "http"))]
        let _ = (saved, name);
        Ok(())
    }

    /// The ingress's counters in the Prometheus text format; empty without
    /// an ingress.
    #[cfg(not(feature = "http"))]
    pub(super) fn metrics(&self) -> String {
        String::new()
    }

    /// The ingress's counters in the Prometheus text format; empty without
    /// an ingress.
    #[cfg(feature = "http")]
    pub(super) fn metrics(&self) -> String {
        let mut text = String::new();
        if let Some(http) = &self.http {
            let s = http.stats();
            for (name, help, value) in [
                (
                    "paddock_http_requests_total",
                    "HTTP requests answered.",
                    s.requests,
                ),
                (
                    "paddock_http_server_errors_total",
                    "HTTP requests answered with a 5xx status.",
                    s.server_errors,
                ),
                (
                    "paddock_http_not_found_total",
                    "HTTP requests that matched no route.",
                    s.not_found,
                ),
                (
                    "paddock_http_redirected_total",
                    "HTTP requests redirected to the failover partner.",
                    s.redirected,
                ),
                (
                    "paddock_http_forwarded_total",
                    "HTTP requests forwarded to the failover partner.",
                    s.forwarded,
                ),
                (
                    "paddock_http_handler_failures_total",
                    "HTTP handler instances that trapped, ran out of time or set no response.",
                    s.handler_failures,
                ),
                (
                    "paddock_http_refused_connections_total",
                    "Connections closed on accept, over max_connections.",
                    s.refused,
                ),
            ] {
                text.push_str(&format!(
                    "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
                ));
            }
        }
        text
    }

    /// Reload every HTTPS ingress's certificates from their files: how many
    /// there are, and each that failed (it keeps its old certificates).
    pub(super) fn reload_tls(&self) -> (usize, Vec<String>) {
        #[cfg(feature = "tls")]
        return (
            self.certificates.len(),
            self.certificates
                .iter()
                .filter_map(|(name, tls)| tls.reload().err().map(|e| format!("http.{name}: {e}")))
                .collect(),
        );
        #[cfg(not(feature = "tls"))]
        (0, Vec::new())
    }

    pub(super) fn stop(&self) {
        #[cfg(feature = "http")]
        if let Some(http) = &self.http {
            http.stop();
        }
    }
}

/// Whether the operator granted an app its HTTP routes.
fn granted_routes(spec: &InstanceSpec) -> bool {
    spec.granted
        .contains(&paddock_bundle::manifest::Capability::HttpIncoming)
}

/// Reload `tls` whenever its files change, checking every `every`, until
/// `stop`.
#[cfg(feature = "tls")]
async fn watch_certificates(
    name: String,
    tls: IngressTls,
    every: Duration,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.wait_for(|s| *s) => return,
            () = tokio::time::sleep(every) => {}
        }
        match tls.reload_if_changed() {
            Ok(true) => tracing::info!(ingress = name, "TLS certificates reloaded"),
            Ok(false) => {}
            Err(e) => {
                tracing::error!(ingress = name, error = %e, "TLS reload failed; keeping the old certificates")
            }
        }
    }
}
