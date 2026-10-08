//! Deterministic runs: runtimes joined by an in-memory mesh, on a
//! single-threaded tokio runtime whose clock is paused.
//!
//! With the clock paused, time moves only when every task waits on a timer,
//! and then jumps straight to the next one: a 30 ms frame delay costs no real
//! time, and the order in which tasks run depends only on what they do. The
//! mesh draws its faults from the seed, so a run is repeatable from its seed.
//!
//! Two departures from the other host tests, both because tokio's paused
//! clock needs a `current_thread` runtime:
//!
//! - CLAUDE.md requires the multi-threaded runtime for instances, since a
//!   guest that yields every epoch tick starves timers on `current_thread`
//!   (S1). These runs use no spinning guest, so nothing yields that way.
//! - The epoch ticker is a real-time OS thread. A tick landing inside a
//!   callback would yield it and reorder ready tasks, so the tick is an hour:
//!   no preemption, and no callback deadline in practice.
//!
//! Heartbeats are faster than a node's defaults ([`HEARTBEAT`] and
//! [`NODE_TIMEOUT`]), so detection tests stay short in virtual time.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use paddock_host::transport::liveness::NodeEvent;
use paddock_host::transport::memory::Mesh;
use paddock_host::{InstanceHandle, LoadedApp, Runtime, RuntimeConfig};
use tokio::sync::broadcast;

use crate::common::{fixture, spec};

/// Heartbeat interval on every simulated node.
pub const HEARTBEAT: Duration = Duration::from_millis(100);
/// Silence after which a simulated node counts a path as silent.
pub const NODE_TIMEOUT: Duration = Duration::from_millis(500);

/// The chaos component's bytes, read once.
fn chaos() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| fixture("paddock_fixture_chaos"))
}

/// One node: a runtime running the chaos app.
pub struct SimNode {
    pub rt: Runtime,
    app: LoadedApp,
}

impl SimNode {
    fn new(node: u32) -> Self {
        let rt = Runtime::new(RuntimeConfig {
            node,
            max_instances: 32,
            epoch_tick: Duration::from_secs(3600),
            heartbeat_interval: HEARTBEAT,
            node_timeout: NODE_TIMEOUT,
            ..RuntimeConfig::default()
        })
        .unwrap();
        let app = rt.load(chaos()).unwrap();
        Self { rt, app }
    }

    /// Start a chaos instance with `init_args` (`trap-exits`, or empty).
    pub fn spawn(&self, init_args: &str) -> InstanceHandle {
        self.rt.spawn(&self.app, spec(init_args)).unwrap()
    }
}

/// Nodes 1 to n, joined by `mesh`.
pub struct Sim {
    pub mesh: Mesh,
    pub nodes: Vec<SimNode>,
}

impl Sim {
    /// Node `id` (1-based, as pids name it).
    pub fn node(&self, id: u32) -> &SimNode {
        &self.nodes[usize::try_from(id - 1).unwrap()]
    }
}

/// Run `scenario` on `nodes` fresh nodes joined by a mesh seeded with
/// `seed`, on a paused clock, and return what it returns.
pub fn simulate<F, Fut, T>(seed: u64, nodes: u32, scenario: F) -> T
where
    F: FnOnce(Sim) -> Fut,
    Fut: Future<Output = T>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async move {
        let mesh = Mesh::new(seed);
        let nodes = (1..=nodes)
            .map(|id| {
                let node = SimNode::new(id);
                mesh.join(&node.rt).unwrap();
                node
            })
            .collect();
        scenario(Sim { mesh, nodes }).await
    })
}

/// The events received so far, without waiting.
pub fn drain(events: &mut broadcast::Receiver<NodeEvent>) -> Vec<NodeEvent> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        out.push(event);
    }
    out
}

/// Let virtual time pass.
pub async fn pass(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}
