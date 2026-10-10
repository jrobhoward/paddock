//! Whether other nodes are alive and reachable: heartbeats, sessions, and
//! the events that failover reads (ROADMAP C5).
//!
//! Every `heartbeat_interval` (see [`RuntimeConfig`](crate::RuntimeConfig)),
//! a runtime sends each peer a [`Frame::Heartbeat`](super::Frame) over its
//! transport, and a beat on each extra [`HeartbeatPath`] (a serial line, for
//! instance). Any frame from a peer counts as hearing it on the cluster link
//! ([`LINK`]). A path not heard for `node_timeout` is silent.
//!
//! A peer is reachable while its link is not silent and its last heartbeat
//! said it hears this node. When a peer stops being reachable, the runtime
//! ends the session with it: links and monitors with its pids end with
//! `noconnection`, calls waiting on it fail, and frames to it are refused
//! until it is reachable again.
//!
//! Sessions make both sides agree that the old one ended. Every frame
//! carries the sender's session number for the pair; ending a session raises
//! it. A frame from an older session is discarded, so nothing sent before
//! the end is acted on after it. A frame from a newer session tells the
//! receiver that the other side ended the old one, and the receiver ends it
//! too before handling the frame. A one-way failure therefore ends the
//! session on both sides: the side that stops hearing ends it, and its
//! heartbeats, which still get through, carry the new number to the other.
//!
//! Every beat, on the link and on each extra path, carries a small payload
//! the node sets ([`crate::failover`] puts the node's role and epoch there),
//! and the receiver keeps the latest one per path. Extra paths only report
//! [`NodeEvent::PathSilent`] and [`NodeEvent::PathHeard`]; they carry no
//! frames and never end a session.
//! Failover (ROADMAP D10) reads them to tell a partition (link silent, serial
//! heard) from a dead node (both silent).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::time::{Instant, MissedTickBehavior};

use super::{Frame, Packet};
use crate::runtime::RuntimeInner;

/// The name of the cluster link (the transport) in [`NodeEvent`]s and
/// [`PeerStatus`].
pub const LINK: &str = "link";

/// A way, besides the transport, to tell another node this one is alive.
///
/// The receiving side reports each beat with
/// [`Runtime::heard`](crate::Runtime::heard).
pub trait HeartbeatPath: Send + Sync + 'static {
    /// Tell `node` this node is alive, with this node's beat payload.
    /// Returns `false` if the path cannot reach it now.
    fn beat(&self, node: u32, payload: &[u8]) -> bool;
}

/// A change in what this node knows about another.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NodeEvent {
    /// The session with `node` ended (OTP's `nodedown`): its links and
    /// monitors ended with `noconnection` and calls waiting on it failed.
    /// `session` is the number of the session that follows.
    Down {
        /// The other node.
        node: u32,
        /// The new session number.
        session: u64,
    },
    /// `node` stopped being reachable on the cluster link; frames to it are
    /// refused.
    Unreachable {
        /// The other node.
        node: u32,
    },
    /// `node` is reachable again, in both directions.
    Reachable {
        /// The other node.
        node: u32,
    },
    /// `node` became a member, or restarted with a new `creation`
    /// ([`super::membership`]).
    Joined {
        /// The other node.
        node: u32,
        /// Its creation.
        creation: u32,
    },
    /// `node` left the cluster.
    Left {
        /// The other node.
        node: u32,
    },
    /// No beat from `node` on `path` for `node_timeout`.
    PathSilent {
        /// The other node.
        node: u32,
        /// The path's name ([`LINK`] for the transport).
        path: String,
    },
    /// A beat from `node` on `path` after it was silent.
    PathHeard {
        /// The other node.
        node: u32,
        /// The path's name ([`LINK`] for the transport).
        path: String,
    },
}

/// What this node knows about another.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PeerStatus {
    /// The current session number.
    pub session: u64,
    /// Whether frames to it are sent.
    pub reachable: bool,
    /// Each path ([`LINK`] first) and whether it was heard within
    /// `node_timeout`.
    pub paths: Vec<(String, bool)>,
}

/// One path to a peer.
#[derive(Debug, Clone)]
struct PathState {
    heard: Instant,
    silent: bool,
    /// The payload of the last beat heard on it, and when. On the link,
    /// other frames count as hearing it but carry no payload.
    payload: Vec<u8>,
    payload_at: Instant,
}

impl PathState {
    fn new(now: Instant) -> Self {
        Self {
            heard: now,
            silent: false,
            payload: Vec::new(),
            payload_at: now,
        }
    }

    fn set_payload(&mut self, payload: Vec<u8>, now: Instant) {
        self.payload = payload;
        self.payload_at = now;
    }
}

/// The last beat heard from a peer on one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Beat {
    pub(crate) path: String,
    /// A beat (not just any frame) heard within `node_timeout`.
    pub(crate) live: bool,
    /// When the last beat, and so `payload`, arrived.
    pub(crate) heard: Instant,
    pub(crate) payload: Vec<u8>,
}

#[derive(Debug)]
struct Peer {
    /// Its creation, once a frame from it has been seen.
    creation: Option<u32>,
    session: u64,
    link: PathState,
    /// What its last heartbeat said: whether it hears this node.
    hears_me: bool,
    reachable: bool,
    paths: BTreeMap<String, PathState>,
}

/// How a frame's session compares with this side's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Session {
    /// From a session this side already ended: discard.
    Stale,
    Current,
    /// The other side ended the session, or restarted; this side must end
    /// the old one too. Carries the new number, already recorded.
    Newer(u64),
}

/// Liveness of every peer, and the heartbeat task.
pub(crate) struct Liveness {
    interval: Duration,
    timeout: Duration,
    peers: Mutex<BTreeMap<u32, Peer>>,
    paths: RwLock<Vec<(String, Arc<dyn HeartbeatPath>)>>,
    events: broadcast::Sender<NodeEvent>,
    started: AtomicBool,
    /// What this node's beats carry.
    payload: RwLock<Vec<u8>>,
}

impl std::fmt::Debug for Liveness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Liveness")
            .field("interval", &self.interval)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Liveness {
    pub(crate) fn new(interval: Duration, timeout: Duration) -> Self {
        Self {
            interval,
            timeout,
            peers: Mutex::new(BTreeMap::new()),
            paths: RwLock::new(Vec::new()),
            events: broadcast::channel(1024).0,
            started: AtomicBool::new(false),
            payload: RwLock::new(Vec::new()),
        }
    }

    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    /// Set what this node's beats carry from now on.
    pub(crate) fn set_payload(&self, payload: Vec<u8>) {
        *self.payload.write().unwrap_or_else(PoisonError::into_inner) = payload;
    }

    fn payload(&self) -> Vec<u8> {
        self.payload
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The last beat from `node` on each path, the link first. Empty if
    /// `node` is not watched.
    pub(crate) fn beats_from(&self, node: u32) -> Vec<Beat> {
        let peers = self.peers();
        let Some(peer) = peers.get(&node) else {
            return Vec::new();
        };
        let now = Instant::now();
        let beat = |path: &str, s: &PathState| Beat {
            path: path.to_string(),
            live: now.duration_since(s.payload_at) <= self.timeout,
            heard: s.payload_at,
            payload: s.payload.clone(),
        };
        let mut out = vec![beat(LINK, &peer.link)];
        out.extend(peer.paths.iter().map(|(name, s)| beat(name, s)));
        out
    }

    fn peers(&self) -> MutexGuard<'_, BTreeMap<u32, Peer>> {
        self.peers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn path_names(&self) -> Vec<String> {
        self.paths
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// `node`'s entry, created on first use as if just heard.
    fn peer<'a>(
        &self,
        peers: &'a mut BTreeMap<u32, Peer>,
        node: u32,
        now: Instant,
    ) -> &'a mut Peer {
        peers.entry(node).or_insert_with(|| Peer {
            creation: None,
            session: 0,
            link: PathState::new(now),
            hears_me: true,
            reachable: true,
            paths: self
                .path_names()
                .into_iter()
                .map(|name| (name, PathState::new(now)))
                .collect(),
        })
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.events.subscribe()
    }

    fn emit(&self, events: Vec<NodeEvent>) {
        for event in events {
            // No receivers is fine.
            let _ = self.events.send(event);
        }
    }

    /// Watch `node`, and start the heartbeat task if it is not running.
    pub(crate) fn add_peer(&self, rt: &Arc<RuntimeInner>, node: u32) {
        if node == rt.node() {
            return;
        }
        {
            let mut peers = self.peers();
            self.peer(&mut peers, node, Instant::now());
        }
        if !self.interval.is_zero()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
            && !self.started.swap(true, Ordering::SeqCst)
        {
            handle.spawn(run(Arc::downgrade(rt), self.interval));
        }
    }

    /// Stop watching `node`.
    pub(crate) fn remove_peer(&self, node: u32) {
        self.peers().remove(&node);
    }

    /// `node`'s creation, if a frame from it has been seen; 0 if not.
    pub(crate) fn creation(&self, node: u32) -> u32 {
        self.peers()
            .get(&node)
            .and_then(|p| p.creation)
            .unwrap_or(0)
    }

    pub(crate) fn add_path(&self, name: &str, path: Arc<dyn HeartbeatPath>) {
        self.paths
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((name.to_string(), path));
        let now = Instant::now();
        for peer in self.peers().values_mut() {
            peer.paths
                .entry(name.to_string())
                .or_insert_with(|| PathState::new(now));
        }
    }

    /// The session number frames to `node` carry.
    pub(crate) fn session(&self, node: u32) -> u64 {
        self.peers().get(&node).map_or(0, |p| p.session)
    }

    /// Whether frames to `node` may be sent. Nodes not watched are left to
    /// the transport.
    pub(crate) fn reachable(&self, node: u32) -> bool {
        self.peers().get(&node).is_none_or(|p| p.reachable)
    }

    pub(crate) fn status(&self, node: u32) -> Option<PeerStatus> {
        let peers = self.peers();
        let peer = peers.get(&node)?;
        let mut paths = vec![(LINK.to_string(), !peer.link.silent)];
        paths.extend(peer.paths.iter().map(|(n, s)| (n.clone(), !s.silent)));
        Some(PeerStatus {
            session: peer.session,
            reachable: peer.reachable,
            paths,
        })
    }

    /// Raise the session with `node` and return the new number. A node no
    /// longer watched (it left) stays unwatched: recreating it here, with a
    /// raised number and no creation, would make the first frames of its
    /// next start look stale, and its join would be dropped.
    pub(crate) fn end(&self, node: u32) -> u64 {
        let mut peers = self.peers();
        let Some(peer) = peers.get_mut(&node) else {
            return 0;
        };
        peer.session += 1;
        peer.session
    }

    /// A frame from `node`'s creation `creation` carrying `session` arrived:
    /// it counts as hearing the link. Returns how its session compares. A
    /// higher creation than before is a restart: its numbering starts
    /// afresh, and the old session must end.
    pub(crate) fn on_frame(&self, node: u32, creation: u32, session: u64) -> Session {
        let mut events = Vec::new();
        let result = {
            let mut peers = self.peers();
            let peer = self.peer(&mut peers, node, Instant::now());
            peer.link.heard = Instant::now();
            if peer.link.silent {
                peer.link.silent = false;
                events.push(NodeEvent::PathHeard {
                    node,
                    path: LINK.to_string(),
                });
            }
            if let Some(known) = peer.creation
                && creation != known
            {
                if creation < known {
                    Session::Stale
                } else {
                    peer.creation = Some(creation);
                    peer.session = session;
                    Session::Newer(session)
                }
            } else if session < peer.session {
                peer.creation = Some(creation);
                Session::Stale
            } else if session > peer.session {
                peer.creation = Some(creation);
                peer.session = session;
                Session::Newer(session)
            } else {
                peer.creation = Some(creation);
                Session::Current
            }
        };
        self.emit(events);
        result
    }

    /// `node`'s heartbeat said whether it hears this node. Returns `true` if
    /// `node` just stopped being reachable, so the session must end.
    pub(crate) fn on_heartbeat(&self, node: u32, hears_me: bool, payload: Vec<u8>) -> bool {
        let (events, lost) = {
            let mut peers = self.peers();
            let peer = self.peer(&mut peers, node, Instant::now());
            peer.hears_me = hears_me;
            peer.link.set_payload(payload, Instant::now());
            update(node, peer)
        };
        self.emit(events);
        lost
    }

    /// A beat from `node` on the extra path `path`.
    pub(crate) fn on_beat(&self, node: u32, path: &str, payload: &[u8]) {
        let mut events = Vec::new();
        {
            let now = Instant::now();
            let mut peers = self.peers();
            let peer = self.peer(&mut peers, node, now);
            if let Some(state) = peer.paths.get_mut(path) {
                state.heard = now;
                state.set_payload(payload.to_vec(), now);
                if state.silent {
                    state.silent = false;
                    events.push(NodeEvent::PathHeard {
                        node,
                        path: path.to_string(),
                    });
                }
            }
        }
        self.emit(events);
    }

    /// What to send this round: each peer, its creation as known here (0
    /// if not yet), its session, and whether this side hears it.
    fn beats(&self) -> Vec<(u32, u32, u64, bool)> {
        self.peers()
            .iter()
            .map(|(&node, p)| (node, p.creation.unwrap_or(0), p.session, !p.link.silent))
            .collect()
    }

    /// Mark paths not heard within the timeout as silent. Returns the peers
    /// that just stopped being reachable.
    fn check(&self, now: Instant) -> Vec<u32> {
        let mut events = Vec::new();
        let mut lost = Vec::new();
        {
            let mut peers = self.peers();
            for (&node, peer) in peers.iter_mut() {
                if !peer.link.silent && now.duration_since(peer.link.heard) > self.timeout {
                    peer.link.silent = true;
                    events.push(NodeEvent::PathSilent {
                        node,
                        path: LINK.to_string(),
                    });
                }
                for (name, state) in &mut peer.paths {
                    if !state.silent && now.duration_since(state.heard) > self.timeout {
                        state.silent = true;
                        events.push(NodeEvent::PathSilent {
                            node,
                            path: name.clone(),
                        });
                    }
                }
                let (changes, now_lost) = update(node, peer);
                events.extend(changes);
                if now_lost {
                    lost.push(node);
                }
            }
        }
        self.emit(events);
        lost
    }

    pub(crate) fn announce(&self, event: NodeEvent) {
        self.emit(vec![event]);
    }
}

/// Recompute whether `peer` is reachable. Returns the events, and whether it
/// just stopped being reachable.
fn update(node: u32, peer: &mut Peer) -> (Vec<NodeEvent>, bool) {
    let reachable = !peer.link.silent && peer.hears_me;
    if reachable == peer.reachable {
        return (Vec::new(), false);
    }
    peer.reachable = reachable;
    if reachable {
        (vec![NodeEvent::Reachable { node }], false)
    } else {
        (vec![NodeEvent::Unreachable { node }], true)
    }
}

/// Beat every peer each interval and end sessions with peers that stop
/// being reachable, until the runtime is gone.
async fn run(rt: Weak<RuntimeInner>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(rt) = rt.upgrade() else {
            return;
        };
        round(&rt);
    }
}

fn round(rt: &Arc<RuntimeInner>) {
    let liveness = &rt.liveness;
    let paths: Vec<Arc<dyn HeartbeatPath>> = liveness
        .paths
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(_, p)| p.clone())
        .collect();
    let creation = rt.creation();
    let payload = liveness.payload();
    for (node, to_creation, session, hears_you) in liveness.beats() {
        if let Some(transport) = rt.transport() {
            transport.send(
                node,
                Packet {
                    creation,
                    to_creation,
                    session,
                    frame: Frame::Heartbeat {
                        hears_you,
                        payload: payload.clone(),
                    },
                },
            );
        }
        for path in &paths {
            path.beat(node, &payload);
        }
    }
    for node in liveness.check(Instant::now()) {
        super::end_session(rt, node);
    }
}
