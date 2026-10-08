//! An in-process transport: runtimes in one process joined as nodes of a
//! [`Mesh`], with faults to inject between them.
//!
//! Each directed pair of nodes has its own queue, delivered in order by its
//! own task, so frames between one pair never reorder while pairs interleave
//! freely. Per pair, a frame can be delayed (a fixed delay plus a random
//! jitter, still in order) or dropped at random. A partition cuts a pair:
//! sends fail, frames in flight are lost, and both nodes see the other go
//! down (`noconnection`). Healing reconnects the pair; links and monitors
//! lost to the partition stay lost, as in OTP.
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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{Frame, Transport};
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
    rng: SplitMix64,
}

struct Queue {
    tx: mpsc::UnboundedSender<(Instant, u64, Frame)>,
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

    fn send(self: &Arc<Self>, from: u32, to: u32, frame: Frame) -> bool {
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
        queue.tx.send((due, generation, frame)).is_ok()
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
    mut rx: mpsc::UnboundedReceiver<(Instant, u64, Frame)>,
) {
    while let Some((due, generation, frame)) = rx.recv().await {
        tokio::time::sleep_until(due).await;
        let Some(shared) = shared.upgrade() else {
            return;
        };
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
            super::deliver(&rt, from, frame);
        }
    }
}

/// One node's side of the mesh: the transport its runtime sends through.
struct Port {
    shared: Weak<Shared>,
    node: u32,
}

impl Transport for Port {
    fn send(&self, node: u32, frame: Frame) -> bool {
        self.shared
            .upgrade()
            .is_some_and(|shared| shared.send(self.node, node, frame))
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
                    rng: SplitMix64(seed),
                }),
            }),
        }
    }

    /// Add `rt` as node `rt.node()`, and set its transport to the mesh.
    ///
    /// # Errors
    ///
    /// [`HostError::Engine`] if another runtime in the mesh has the same
    /// node id, or `rt` already has a transport.
    pub fn join(&self, rt: &Runtime) -> Result<(), HostError> {
        let node = rt.node();
        {
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
            state.nodes.insert(node, Arc::downgrade(rt.inner()));
        }
        rt.set_transport(Arc::new(Port {
            shared: Arc::downgrade(&self.shared),
            node,
        }))
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
            super::node_down(&rt, b);
        }
        if let Some(rt) = self.shared.runtime(b) {
            super::node_down(&rt, a);
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

    /// Reconnect `a` and `b`.
    pub fn heal(&self, a: u32, b: u32) {
        self.shared.state().partitioned.remove(&pair(a, b));
    }

    /// Reconnect every pair.
    pub fn heal_all(&self) {
        self.shared.state().partitioned.clear();
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
