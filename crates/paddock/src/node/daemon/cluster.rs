//! The cluster transport, failover and the witness.

use std::time::Duration;

use crate::api::{Done, FailoverInfo, MemberInfo, ReplicationInfo};
use paddock_host::Role;
use paddock_host::failover::{Arbiter, FailoverConfig, SwitchoverRefused, WitnessConfig};
use paddock_host::transport::quic::{NodeIdentity, QuicConfig, QuicTransport};
use serde_json::json;

use super::Node;
use crate::node::config::{ArbiterKind, BucketScope};
use crate::node::error::NodeError;
use crate::node::gate;

impl Node {
    /// The cluster's members as this node knows them, itself included.
    #[must_use]
    pub fn members(&self) -> Vec<MemberInfo> {
        let me = self.rt.node();
        self.rt
            .members()
            .into_iter()
            .map(|m| MemberInfo {
                reachable: m.node == me || self.rt.peer_status(m.node).is_some_and(|s| s.reachable),
                node: m.node,
                creation: m.creation,
                address: m.address,
                role: match m.role {
                    Role::Witness => "witness".into(),
                    _ => "member".into(),
                },
            })
            .collect()
    }

    /// Start the QUIC transport on `cluster.address`, join the seeds, and
    /// watch the identity files.
    pub(super) fn start_cluster(&self) -> Result<(), NodeError> {
        let cluster = &self.config.cluster;
        let (Some(address), Some(key), Some(peers)) =
            (&cluster.address, &cluster.key, &cluster.peers)
        else {
            return Ok(());
        };
        let bind = address
            .parse()
            .map_err(|e| NodeError::Invalid(format!("cluster.address: {e}")))?;
        let quic = QuicTransport::start(
            &self.rt,
            QuicConfig {
                bind,
                identity: NodeIdentity {
                    key: key.clone(),
                    peers: peers.clone(),
                },
                idle_timeout: Duration::from_millis(cluster.node_timeout_ms.max(1) * 2),
                send_buffer_bytes: cluster.send_buffer_bytes,
            },
        )
        .map_err(|e| NodeError::Invalid(e.to_string()))?;
        tracing::info!(%bind, "cluster transport listening");
        for seed in &cluster.seeds {
            let Ok(seed) = seed.parse() else {
                continue;
            };
            let quic = quic.clone();
            tokio::spawn(async move {
                match quic.join_seed(seed).await {
                    Ok(node) => tracing::info!(%seed, node, "joined through seed"),
                    Err(e) => tracing::warn!(error = %e, "seed not reached yet; retrying"),
                }
            });
        }
        if cluster.identity_reload_s > 0 {
            let (quic, every) = (quic.clone(), Duration::from_secs(cluster.identity_reload_s));
            let mut stop = self.stop.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.wait_for(|s| *s) => return,
                        () = tokio::time::sleep(every) => {}
                    }
                    match quic.reload_if_changed() {
                        Ok(true) => tracing::info!("cluster key or peers reloaded"),
                        Ok(false) => {}
                        Err(e) => {
                            tracing::error!(error = %e, "cluster identity reload failed; keeping the old")
                        }
                    }
                }
            });
        }
        *self
            .quic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(quic);
        Ok(())
    }

    /// Start `[failover]` at the epoch kept on disk and follow its
    /// leadership, or serve `[witness]` from its kept state.
    pub(super) fn start_failover(&self) -> Result<(), NodeError> {
        if let Some(w) = &self.config.witness {
            let mut cfg = WitnessConfig::new(w.pair, w.first);
            cfg.lease = Duration::from_millis(w.lease_ms);
            cfg.margin = Duration::from_millis(w.margin_ms);
            cfg.resume = self.store.load_witness()?;
            self.rt
                .serve_witness_kept(cfg, Some(self.store.witness_persist()))
                .map_err(|e| NodeError::Invalid(format!("witness: {e}")))?;
            tracing::info!(pair = ?w.pair, "serving as witness");
        }
        let (Some(f), Some(gate)) = (&self.config.failover, &self.gate) else {
            return Ok(());
        };
        let arbiter = match (f.arbiter, f.witness) {
            (ArbiterKind::Witness, Some(w)) => Arbiter::Witness(w),
            (ArbiterKind::Operator, _) => Arbiter::Operator,
            _ => Arbiter::Paths,
        };
        #[cfg(unix)]
        if let Some(serial) = &f.serial {
            use crate::node::serial_path::{self, Line, PATH};
            let line = Line {
                device: serial.device.clone(),
                baud: serial.baud,
            };
            let path = serial_path::start(
                self.rt.clone(),
                self.config.node,
                f.partner,
                line,
                self.stop.subscribe(),
            );
            self.rt.add_heartbeat_path(PATH, path);
        }
        let epoch = self.store.load_epoch()?;
        let mut cfg = FailoverConfig::new(f.partner, arbiter);
        cfg.lease = Duration::from_millis(f.lease_ms);
        cfg.margin = Duration::from_millis(f.margin_ms);
        cfg.epoch = epoch;
        cfg.replication_log_bytes = f.replication_log_max.bytes();
        cfg.replication_log_age = Duration::from_secs(f.replication_log_age_s);
        let handle = self
            .rt
            .start_failover(cfg)
            .map_err(|e| NodeError::Invalid(format!("failover: {e}")))?;
        tracing::info!(partner = f.partner, epoch, "failover started");
        #[cfg(feature = "http")]
        {
            // A singleton runs on the partner while this node is its
            // standby and hears it.
            let (gate, rt, partner) = (gate.clone(), self.rt.clone(), f.partner);
            let leadership = handle.subscribe();
            self.ingress.set_elsewhere(std::sync::Arc::new(move |app| {
                (leadership.borrow().standing == paddock_host::failover::Standing::Standby
                    && gate.holds(app)
                    && rt.peer_status(partner).is_some_and(|s| s.reachable))
                .then_some(partner)
            }));
        }
        tokio::spawn(gate::follow(
            gate.clone(),
            self.rt.clone(),
            self.store.epoch_saver(),
            epoch,
            handle.subscribe(),
            self.stop.subscribe(),
        ));
        let mut changes = handle.subscribe();
        let (generation, mut stop) = (self.leadership.clone(), self.stop.subscribe());
        let events = self.events.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = changes.changed() => if changed.is_err() { return },
                    _ = stop.wait_for(|s| *s) => return,
                }
                generation.send_modify(|g| *g += 1);
                let now = changes.borrow_and_update().clone();
                events.emit(
                    "failover",
                    "leadership",
                    json!({
                        "role": gate::role_name(now.standing),
                        "epoch": now.epoch,
                        "may_run": now.may_run,
                        "awaiting_confirmation": now.awaiting_confirmation,
                        "generation": *generation.borrow(),
                    }),
                );
            }
        });
        let _ = self.failover.set(handle);
        Ok(())
    }

    /// This node's failover standing.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`.
    pub fn failover(&self) -> Result<FailoverInfo, NodeError> {
        let (Some(f), Some(handle)) = (&self.config.failover, self.failover.get()) else {
            return Err(NodeError::NotFound(
                "this node has no [failover] in its config".into(),
            ));
        };
        let now = handle.leadership();
        let singletons = self.gate.as_ref().map(|g| g.names()).unwrap_or_default();
        let status = self.rt.peer_status(f.partner);
        Ok(FailoverInfo {
            partner: f.partner,
            arbiter: match f.arbiter {
                ArbiterKind::Witness => format!("witness {}", f.witness.unwrap_or_default()),
                ArbiterKind::Paths => "paths".into(),
                ArbiterKind::Operator => "operator".into(),
            },
            role: gate::role_name(now.standing).into(),
            epoch: now.epoch,
            may_run: now.may_run,
            awaiting_confirmation: now.awaiting_confirmation,
            partner_reachable: status.as_ref().is_some_and(|s| s.reachable),
            paths: status
                .map(|s| s.paths.into_iter().collect())
                .unwrap_or_default(),
            singletons,
            generation: *self.leadership.borrow(),
            partner_outdated: now.partner_outdated,
            replication: self.rt.replication_status().map(|s| ReplicationInfo {
                buckets: self
                    .config
                    .buckets
                    .iter()
                    .filter(|(_, b)| b.scope == BucketScope::Cluster)
                    .map(|(name, _)| name.clone())
                    .collect(),
                commit: s.commit,
                last: s.last,
                base: s.base,
                log_bytes: s.log_bytes,
                partner_matched: s.partner_matched,
                alone: s.alone,
            }),
        })
    }

    /// This node's failover standing once its leadership has changed since
    /// `after` (a [`FailoverInfo::generation`]), or after `wait` without a
    /// change: a long poll, for `paddock failover --watch`.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`.
    pub async fn failover_after(
        &self,
        after: u64,
        wait: Duration,
    ) -> Result<FailoverInfo, NodeError> {
        self.failover()?;
        let mut generation = self.leadership.subscribe();
        let _ = tokio::time::timeout(wait, generation.wait_for(|g| *g > after)).await;
        self.failover()
    }

    /// A planned switchover (ROADMAP D10b): this primary stops its
    /// singletons gracefully, while its lease holds, and hands over; its
    /// standby takes over within about a lease, with no node timeout, and
    /// this node rejoins as the new primary's standby. Refused, with nothing
    /// stopped, unless this node is a primary with a lease, its arbiter is a
    /// witness and its partner is heard as a confirming standby.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`; [`NodeError::Conflict`]
    /// when the switchover is refused.
    pub async fn switchover(&self) -> Result<Done, NodeError> {
        let (Some(handle), Some(gate)) = (self.failover.get(), &self.gate) else {
            return Err(NodeError::NotFound(
                "this node has no [failover] in its config".into(),
            ));
        };
        let refused =
            |e: SwitchoverRefused| NodeError::Conflict(format!("switchover refused: {e}"));
        handle.check_switchover().await.map_err(refused)?;
        let epoch = handle.leadership().epoch;
        gate.hand_over().await;
        if let Err(e) = handle.switchover().await {
            // Changed between the check and now: run again what was stopped.
            gate.restore(handle.leadership().may_run).await;
            return Err(refused(e));
        }
        self.audit
            .record("failover_switchover", json!({ "epoch": epoch }));
        Ok(Done {
            message: "handing over: the singletons stopped here, and the partner takes over \
                      once the witness grants it (about a lease); this node then rejoins as its \
                      standby"
                .into(),
        })
    }

    /// An operator confirms that the partner is gone, so the standby may
    /// promote itself (`paddock_host::failover::FailoverHandle::confirm`).
    /// Taken only while the node awaits a confirmation: with
    /// `arbiter = "operator"`, or a witness that cannot be reached. Safe
    /// only if the partner is down or cut off.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`.
    pub fn confirm_failover(&self) -> Result<Done, NodeError> {
        let handle = self.failover.get().ok_or_else(|| {
            NodeError::NotFound("this node has no [failover] in its config".into())
        })?;
        let now = handle.leadership();
        handle.confirm();
        self.audit.record(
            "failover_confirm",
            json!({ "epoch": now.epoch, "awaiting": now.awaiting_confirmation }),
        );
        Ok(Done {
            message: if now.awaiting_confirmation {
                "confirmed: this node is promoted once the partner's lease has run out".into()
            } else {
                "ignored: this node was not waiting for a confirmation (with a witness, one is \
                 needed only while the witness cannot be reached)"
                    .into()
            },
        })
    }
}
