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

use paddock_host::acl::Acl;
use paddock_host::transport::liveness::NodeEvent;
use paddock_host::transport::memory::Mesh;
use paddock_host::{InstanceHandle, LoadedApp, Role, Runtime, RuntimeConfig};
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
        Self::start(node, 1, Role::Member)
    }

    /// Start node `node`'s `creation` with `role`, not yet in the mesh.
    pub fn start(node: u32, creation: u32, role: Role) -> Self {
        let rt = Runtime::new(RuntimeConfig {
            node,
            creation,
            role,
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

    /// Start a chaos instance as app `app` with messaging routes `acl`.
    pub fn spawn_as(&self, app: &str, init_args: &str, acl: Acl) -> InstanceHandle {
        let mut s = spec(init_args);
        s.app = app.into();
        s.acl = acl;
        self.rt.spawn(&self.app, s).unwrap()
    }

    /// Start a chaos instance bound to the node's failover lease, as a
    /// singleton is, with no gate to start or kill it.
    pub fn spawn_bound(&self) -> InstanceHandle {
        let mut spec = spec("");
        spec.bound_to_lease = true;
        spec.init_timeout = Duration::from_secs(3600);
        self.rt.spawn(&self.app, spec).unwrap()
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
        let sim = Sim { mesh, nodes };
        until_contacted(&sim).await;
        scenario(sim).await
    })
}

/// Wait until every node has heard every other: links and monitors with a
/// node not yet heard fail with `noconnection`.
async fn until_contacted(sim: &Sim) {
    let ids: Vec<u32> = sim.nodes.iter().map(|n| n.rt.node()).collect();
    while !sim.nodes.iter().all(|n| {
        ids.iter()
            .filter(|&&id| id != n.rt.node())
            .all(|&id| n.rt.peer_status(id).is_some_and(|s| s.contacted))
    }) {
        pass(1).await;
    }
}

/// The events received so far, without waiting.
pub fn drain(events: &mut broadcast::Receiver<NodeEvent>) -> Vec<NodeEvent> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        out.push(event);
    }
    out
}

/// Like [`simulate`], but the nodes are only attached to the mesh, with
/// the roles given: none is anyone's peer until it joins the cluster.
pub fn simulate_attached<F, Fut, T>(seed: u64, roles: &[Role], scenario: F) -> T
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
        let nodes = (1..)
            .zip(roles)
            .map(|(id, &role)| {
                let node = SimNode::start(id, 1, role);
                mesh.attach(&node.rt).unwrap();
                node
            })
            .collect();
        scenario(Sim { mesh, nodes }).await
    })
}

/// Let virtual time pass.
pub async fn pass(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}
