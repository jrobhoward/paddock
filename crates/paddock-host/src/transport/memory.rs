//! An in-process transport: runtimes in one process joined as nodes of a
//! [`Mesh`], with faults to inject between them.
//!
//! Each directed pair of nodes has its own queue, delivered in order by its
//! own task, so frames between one pair never reorder while pairs interleave
//! freely. Per pair, a frame can be delayed (a fixed delay plus a random
//! jitter, still in order) or dropped at random. A partition cuts a pair:
//! sends fail, frames in flight are lost, and both nodes see the other go
//! down (`noconnection`) at once, as when a connection is closed. Healing
//! reconnects the pair; links and monitors lost to the partition stay lost,
//! as in OTP.
//!
//! A stall is the silent failure: frames between the pair (in one direction
//! or both) are held, as QUIC holds unacknowledged data, and nobody is told.
//! The nodes' heartbeats ([`super::liveness`]) find it. Healing releases the
//! held frames in order; any from a session that ended meanwhile are
//! discarded by the receiver.
//!
//! [`Mesh::join`] makes every node a peer of every other, so each sends
//! heartbeats, as a configured cluster would. [`Mesh::attach`] only connects
//! the node, for tests of membership ([`super::membership`]). [`Mesh::add_path`] adds a heartbeat path besides the frames
//! (a serial line between nodes), which [`Mesh::cut_path`] cuts on its own.
//!
//! Every random choice comes from one seeded generator, drawn when a frame
//! is sent, so a test that sends in a fixed order gets the same faults on
//! every run.
//!
//! ```no_run
//! # async fn demo(a: paddock_host::Runtime, b: paddock_host::Runtime) -> Result<(), paddock_host::HostError> {
//! use std::time::Duration;
//! use paddock_host::transport::memory::{Faults, Mesh};
//!
//! let mesh = Mesh::new(42);
//! mesh.join(&a)?;
//! mesh.join(&b)?;
//! mesh.set_faults(a.node(), b.node(), Faults { delay: Duration::from_millis(5), ..Faults::default() });
//! mesh.partition(a.node(), b.node());
//! mesh.heal(a.node(), b.node());
//! # Ok(()) }
//! ```

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use super::liveness::HeartbeatPath;
use super::{Frame, Packet, Transport};
use crate::error::HostError;
use crate::runtime::{Runtime, RuntimeInner};

/// Faults on frames from one node to another.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Faults {
    /// Added to every frame's delivery time.
    pub delay: Duration,
    /// Up to this much more, chosen at random per frame. Frames still
    /// arrive in order.
    pub jitter: Duration,
    /// The chance, from 0 to 1, that a frame is lost without the pair going
    /// down.
    pub drop: f64,
}

/// Runtimes in one process, joined as nodes.
#[derive(Clone)]
pub struct Mesh {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Mesh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mesh").finish_non_exhaustive()
    }
}

struct Shared {
    state: Mutex<State>,
    /// Woken when stalls are healed.
    healed: Notify,
}

struct State {
    nodes: HashMap<u32, Weak<RuntimeInner>>,
    queues: HashMap<(u32, u32), Queue>,
    faults: HashMap<(u32, u32), Faults>,
    default_faults: Faults,
    /// Cut pairs, smaller node first.
    partitioned: HashSet<(u32, u32)>,
    /// Bumped per pair at each partition; frames sent before it are lost.
    generation: HashMap<(u32, u32), u64>,
    /// Stalled directions, (from, to): frames wait.
    stalled: HashSet<(u32, u32)>,
    /// Frames to lose once, by direction: the next frame each predicate
    /// matches is dropped, and the predicate removed.
    lose: Vec<(u32, u32, LoseIf)>,
    /// Extra heartbeat paths, by name.
    paths: BTreeSet<String>,
    /// Cut directions of extra paths: (path, from, to).
    path_cut: HashSet<(String, u32, u32)>,
    rng: SplitMix64,
}

/// Which frame [`Mesh::lose_next`] drops.
type LoseIf = Box<dyn Fn(&Frame) -> bool + Send>;

struct Queue {
    tx: mpsc::UnboundedSender<(Instant, u64, Packet)>,
    /// When the last frame queued is due, so a later one is never earlier.
    last_due: Instant,
}

fn pair(a: u32, b: u32) -> (u32, u32) {
    (a.min(b), a.max(b))
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(self: &Arc<Self>, from: u32, to: u32, packet: Packet) -> bool {
        let mut state = self.state();
        if state.partitioned.contains(&pair(from, to))
            || !state.nodes.get(&to).is_some_and(|rt| rt.strong_count() > 0)
        {
            return false;
        }
        let faults = state
            .faults
            .get(&(from, to))
            .copied()
            .unwrap_or(state.default_faults);
        if faults.drop > 0.0 && state.rng.unit() < faults.drop {
            return true;
        }
        if let Some(i) = state
            .lose
            .iter()
            .position(|(f, t, matches)| *f == from && *t == to && matches(&packet.frame))
        {
            drop(state.lose.remove(i));
            return true;
        }
        let jitter = if faults.jitter.is_zero() {
            Duration::ZERO
        } else {
            faults.jitter.mul_f64(state.rng.unit())
        };
        let generation = state.generation.get(&pair(from, to)).copied().unwrap_or(0);
        let now = Instant::now();
        let queue = match state.queues.entry((from, to)) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let Ok(handle) = tokio::runtime::Handle::try_current() else {
                    return false;
                };
                let (tx, rx) = mpsc::unbounded_channel();
                handle.spawn(carry(Arc::downgrade(self), from, to, rx));
                e.insert(Queue { tx, last_due: now })
            }
        };
        let due = (now + faults.delay + jitter).max(queue.last_due);
        queue.last_due = due;
        queue.tx.send((due, generation, packet)).is_ok()
    }

    fn stalled(&self, from: u32, to: u32) -> bool {
        self.state().stalled.contains(&(from, to))
    }

    /// A beat from `from` to `to` on the extra path `path`: arrives at once
    /// unless the path is cut that way.
    fn beat(&self, path: &str, from: u32, to: u32, payload: &[u8]) -> bool {
        let target = {
            let state = self.state();
            if state.path_cut.contains(&(path.to_string(), from, to)) {
                return true;
            }
            state.nodes.get(&to).and_then(Weak::upgrade)
        };
        target.is_some_and(|rt| {
            rt.liveness.on_beat(from, path, payload);
            true
        })
    }

    /// The runtime for `node`, if it is in the mesh and alive.
    fn runtime(&self, node: u32) -> Option<Arc<RuntimeInner>> {
        self.state().nodes.get(&node).and_then(Weak::upgrade)
    }
}

/// Deliver the frames `from` sends `to`, in order, each when due.
async fn carry(
    shared: Weak<Shared>,
    from: u32,
    to: u32,
    mut rx: mpsc::UnboundedReceiver<(Instant, u64, Packet)>,
) {
    while let Some((due, generation, packet)) = rx.recv().await {
        tokio::time::sleep_until(due).await;
        let Some(shared) = shared.upgrade() else {
            return;
        };
        loop {
            // Registered before the check, so a heal in between wakes it.
            let healed = shared.healed.notified();
            if !shared.stalled(from, to) {
                break;
            }
            healed.await;
        }
        let target = {
            let state = shared.state();
            let key = pair(from, to);
            let current = state.generation.get(&key).copied().unwrap_or(0);
            if state.partitioned.contains(&key) || current != generation {
                None
            } else {
                state.nodes.get(&to).and_then(Weak::upgrade)
            }
        };
        if let Some(rt) = target {
            super::deliver(&rt, from, packet);
        }
    }
}

/// One node's side of the mesh: the transport its runtime sends through.
struct Port {
    shared: Weak<Shared>,
    node: u32,
}

impl Transport for Port {
    fn send(&self, node: u32, packet: Packet) -> bool {
        self.shared
            .upgrade()
            .is_some_and(|shared| shared.send(self.node, node, packet))
    }
}

/// One node's side of an extra heartbeat path.
struct PathPort {
    shared: Weak<Shared>,
    node: u32,
    name: String,
}

impl HeartbeatPath for PathPort {
    fn beat(&self, node: u32, payload: &[u8]) -> bool {
        self.shared
            .upgrade()
            .is_some_and(|shared| shared.beat(&self.name, self.node, node, payload))
    }
}

impl Mesh {
    /// An empty mesh whose random faults come from `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    nodes: HashMap::new(),
                    queues: HashMap::new(),
                    faults: HashMap::new(),
                    default_faults: Faults::default(),
                    partitioned: HashSet::new(),
                    generation: HashMap::new(),
                    stalled: HashSet::new(),
                    lose: Vec::new(),
                    paths: BTreeSet::new(),
                    path_cut: HashSet::new(),
                    rng: SplitMix64(seed),
                }),
                healed: Notify::new(),
            }),
        }
    }

    /// Add `rt` as node `rt.node()`, set its transport to the mesh, and
    /// make it and every other node peers, with the mesh's extra paths.
    ///
    /// # Errors
    ///
    /// [`HostError::Engine`] if another runtime in the mesh has the same
    /// node id, or `rt` already has a transport.
    pub fn join(&self, rt: &Runtime) -> Result<(), HostError> {
        for other in self.connect(rt)? {
            other.liveness.add_peer(&other, rt.node());
            rt.add_peer(other.node());
        }
        Ok(())
    }

    /// Add `rt` as node `rt.node()` and set its transport to the mesh, with
    /// the mesh's extra paths, but make it no one's peer: it learns the
    /// others by joining the cluster ([`Runtime::join_cluster`]).
    ///
    /// # Errors
    ///
    /// As for [`Mesh::join`].
    pub fn attach(&self, rt: &Runtime) -> Result<(), HostError> {
        self.connect(rt).map(|_| ())
    }

    /// [`Mesh::attach`], returning the other nodes.
    fn connect(&self, rt: &Runtime) -> Result<Vec<Arc<RuntimeInner>>, HostError> {
        let node = rt.node();
        let (others, paths) = {
            let mut state = self.shared.state();
            if state
                .nodes
                .get(&node)
                .is_some_and(|other| other.strong_count() > 0)
            {
                return Err(HostError::Engine(format!(
                    "node {node} is already in the mesh"
                )));
            }
            let others: Vec<Arc<RuntimeInner>> =
                state.nodes.values().filter_map(Weak::upgrade).collect();
            // A node started again: what its previous runtime still had in
            // flight dies with it, as a dead process's connections do.
            if state
                .nodes
                .insert(node, Arc::downgrade(rt.inner()))
                .is_some()
            {
                for other in others.iter().map(|o| o.node()) {
                    *state.generation.entry(pair(node, other)).or_insert(0) += 1;
                }
            }
            (others, state.paths.clone())
        };
        rt.set_transport(Arc::new(Port {
            shared: Arc::downgrade(&self.shared),
            node,
        }))?;
        for name in &paths {
            rt.add_heartbeat_path(name, self.path_port(node, name));
        }
        Ok(others)
    }

    fn path_port(&self, node: u32, name: &str) -> Arc<PathPort> {
        Arc::new(PathPort {
            shared: Arc::downgrade(&self.shared),
            node,
            name: name.to_string(),
        })
    }

    /// Add a heartbeat path named `name` between every pair of nodes, now
    /// and as they join.
    pub fn add_path(&self, name: &str) {
        let nodes: Vec<Arc<RuntimeInner>> = {
            let mut state = self.shared.state();
            if !state.paths.insert(name.to_string()) {
                return;
            }
            state.nodes.values().filter_map(Weak::upgrade).collect()
        };
        for rt in nodes {
            rt.liveness.add_path(name, self.path_port(rt.node(), name));
        }
    }

    /// Cut the extra path `name` between `a` and `b`, both ways.
    pub fn cut_path(&self, a: u32, b: u32, name: &str) {
        let mut state = self.shared.state();
        state.path_cut.insert((name.to_string(), a, b));
        state.path_cut.insert((name.to_string(), b, a));
    }

    /// Reconnect the extra path `name` between `a` and `b`.
    pub fn heal_path(&self, a: u32, b: u32, name: &str) {
        let mut state = self.shared.state();
        state.path_cut.remove(&(name.to_string(), a, b));
        state.path_cut.remove(&(name.to_string(), b, a));
    }

    /// Hold frames between `a` and `b`, both ways, telling nobody.
    pub fn stall(&self, a: u32, b: u32) {
        self.stall_one_way(a, b);
        self.stall_one_way(b, a);
    }

    /// Hold frames from `from` to `to`, telling nobody.
    pub fn stall_one_way(&self, from: u32, to: u32) {
        self.shared.state().stalled.insert((from, to));
    }

    /// Lose the next frame from `from` to `to` that `matches`, telling
    /// nobody (the pair stays up), as a single lost datagram would.
    pub fn lose_next(&self, from: u32, to: u32, matches: impl Fn(&Frame) -> bool + Send + 'static) {
        self.shared.state().lose.push((from, to, Box::new(matches)));
    }

    /// Faults on frames from `from` to `to` (one direction).
    pub fn set_faults(&self, from: u32, to: u32, faults: Faults) {
        self.shared.state().faults.insert((from, to), faults);
    }

    /// Faults on every pair without its own.
    pub fn set_default_faults(&self, faults: Faults) {
        self.shared.state().default_faults = faults;
    }

    /// Cut `a` from `b`: sends between them fail, frames in flight are
    /// lost, and each node sees the other go down. Cutting a cut pair does
    /// nothing.
    pub fn partition(&self, a: u32, b: u32) {
        let key = pair(a, b);
        {
            let mut state = self.shared.state();
            if !state.partitioned.insert(key) {
                return;
            }
            *state.generation.entry(key).or_insert(0) += 1;
        }
        if let Some(rt) = self.shared.runtime(a) {
            super::end_session(&rt, b);
        }
        if let Some(rt) = self.shared.runtime(b) {
            super::end_session(&rt, a);
        }
    }

    /// Cut `node` from every other node in the mesh.
    pub fn isolate(&self, node: u32) {
        let others: Vec<u32> = self
            .shared
            .state()
            .nodes
            .keys()
            .copied()
            .filter(|&n| n != node)
            .collect();
        for other in others {
            self.partition(node, other);
        }
    }

    /// Reconnect `a` and `b` after a partition or a stall; held frames
    /// move on.
    pub fn heal(&self, a: u32, b: u32) {
        {
            let mut state = self.shared.state();
            state.partitioned.remove(&pair(a, b));
            state.stalled.remove(&(a, b));
            state.stalled.remove(&(b, a));
        }
        self.shared.healed.notify_waiters();
    }

    /// Reconnect every pair, and every extra path.
    pub fn heal_all(&self) {
        {
            let mut state = self.shared.state();
            state.partitioned.clear();
            state.stalled.clear();
            state.path_cut.clear();
        }
        self.shared.healed.notify_waiters();
    }
}

/// SplitMix64: small, seedable, good enough for fault injection.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    #[allow(clippy::cast_precision_loss)] // 53 bits fit an f64 exactly.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}
