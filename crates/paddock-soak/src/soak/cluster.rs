//! The cluster soak: three member nodes as `paddock node`
//! processes, each behind a proxy, each running a pool of the chaos
//! fixture. Instances are told, at random, to link, unlink, monitor and
//! demonitor instances on any node, to trap exits or not, and are killed;
//! meanwhile links between nodes are cut, held or lossy, and nodes are
//! killed and restarted, or stopped and continued.
//!
//! After each episode everything is healed and left alone until the nodes
//! hear each other again. Then every node's `ps` must agree (spike S12's
//! `Agreement`): each link, monitor and watched monitor an instance has
//! names a live instance that lists it back. An entry naming an instance
//! that is gone is an exit or down notice that never arrived.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use paddock::api::{InstanceInfo, Target};

use super::events::{EventLog, What};
use super::node::{ADMIN_TIMEOUT, Node, bundle, free_udp, socket_path};
use super::proxy::{self, Faults, Mode};
use super::rng::Rng;

/// The pool's app name.
pub(crate) const APP: &str = "links";

/// Nodes in the cluster.
const NODES: u32 = 3;

/// Instances per node.
const POOL: usize = 4;

/// At most this many faults at once.
const FAULTS_MAX: usize = 2;

/// How long an instance's command may take.
const CALL_MS: u64 = 1000;

/// A cluster run.
#[derive(Debug)]
pub(crate) struct Args {
    pub dir: PathBuf,
    pub root: PathBuf,
    pub paddock: PathBuf,
    pub seed: u64,
    pub minutes: f64,
    pub episode: Duration,
    pub heartbeat_ms: u64,
    pub node_timeout_ms: u64,
}

/// Run, and return whether the run passed.
pub(crate) async fn run(args: &Args) -> anyhow::Result<bool> {
    let events = Arc::new(EventLog::create(&args.dir.join("events.jsonl"))?);
    let deadline = Instant::now() + Duration::from_secs_f64(args.minutes * 60.0);
    let mut rng = Rng::new(args.seed);
    let mut problems = Vec::new();
    let mut checks = 0;
    {
        let mut cluster = Cluster::start(args, events.clone()).await?;
        while Instant::now() < deadline && problems.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            cluster.episode(&mut rng, args.episode.min(left)).await?;
            cluster.heal().await?;
            problems = cluster.agree().await;
            checks += 1;
        }
        // Dropping the cluster kills every node, recorded.
    }
    let mut text = format!("{checks} checks after a heal\n");
    for p in &problems {
        let _ = writeln!(text, "PROBLEM: {p}");
    }
    std::fs::write(args.dir.join("result.txt"), &text)?;
    print!("{text}");
    Ok(problems.is_empty())
}

/// What ends a fault.
#[derive(Debug, Clone, Copy)]
enum Undo {
    Link(u32, u32),
    Thaw(u32),
    Restart(u32),
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Link,
    Kill,
    Freeze,
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Link,
    Unlink,
    Monitor,
    Demonitor,
    Trap,
    KillInstance,
}

/// The three nodes, their proxies and the faults between them.
#[derive(Debug)]
struct Cluster {
    nodes: Vec<Node>,
    faults: Arc<Faults>,
    events: Arc<EventLog>,
    node_timeout: Duration,
    heartbeat: Duration,
    /// Instances last seen on each node, by node.
    known: BTreeMap<u32, Vec<InstanceInfo>>,
}

impl Cluster {
    /// Keys, configs, proxies and the bundle; start the nodes and deploy
    /// the pool to each.
    async fn start(args: &Args, events: Arc<EventLog>) -> anyhow::Result<Self> {
        let dir = &args.dir;
        let mut peers_file = String::from("[peers]\n");
        for n in 1..=NODES {
            let public = super::node::paddock(
                &args.paddock,
                dir,
                &["cluster", "keygen", &format!("node-{n}.key")],
            )?;
            let _ = writeln!(peers_file, "{n} = [\"{}\"]", public.trim());
        }
        std::fs::write(dir.join("peers.toml"), peers_file)?;

        let manifest = format!(
            "[app]\nname = \"{APP}\"\nversion = \"1.0.0\"\n\
             description = \"Links and monitors on command (paddock-fixture-chaos)\"\n\
             license = \"MIT OR Apache-2.0\"\n\
             [appserver.instances]\ncount = {POOL}\n\
             [appserver.limits]\nmemory_max = \"32MiB\"\ncallback_deadline_ms = 500\n\
             [appserver.supervision]\nintensity = 1000\nperiod_ms = 10000\n\
             backoff = {{ initial_ms = 10, max_ms = 200, jitter = true }}\n"
        );
        let wasm = args
            .root
            .join("target/wasm32-wasip2/release/paddock_fixture_chaos.wasm");
        let (app, signer) = bundle(&args.paddock, dir, APP, &manifest, &wasm)?;

        let bound: Vec<_> = (0..NODES).map(|_| free_udp()).collect::<Result<_, _>>()?;
        let names: std::collections::HashMap<_, _> = bound.iter().copied().zip(1..=NODES).collect();
        let faults = Arc::new(Faults::new(args.seed));
        let mut advertised = Vec::new();
        for (n, &address) in (1..=NODES).zip(&bound) {
            advertised.push(proxy::start(n, address, names.clone(), faults.clone()).await?);
        }

        let mut nodes = Vec::new();
        for n in 1..=NODES {
            let i = usize::try_from(n - 1)?;
            let node = Node::new(n, &args.paddock, dir, socket_path("c", n), events.clone());
            let seeds: Vec<String> = advertised
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .map(|(_, a)| format!("\"{a}\""))
                .collect();
            let config = format!(
                "node = {n}\ndata_dir = {:?}\nadmin_socket = {:?}\ntrusted_keys = [{signer:?}]\n\
                 max_instances = 32\n[cluster]\naddress = \"{}\"\nadvertise = \"{}\"\n\
                 key = {:?}\npeers = {:?}\nheartbeat_interval_ms = {}\nnode_timeout_ms = {}\n\
                 seeds = [{}]\n",
                dir.join(format!("data-{n}")),
                node.socket(),
                bound[i],
                advertised[i],
                dir.join(format!("node-{n}.key")),
                dir.join("peers.toml"),
                args.heartbeat_ms,
                args.node_timeout_ms,
                seeds.join(", "),
            );
            std::fs::write(node.config_path(), config)?;
            nodes.push(node);
        }
        for node in &mut nodes {
            node.spawn().await?;
        }
        for node in &nodes {
            tokio::time::timeout(ADMIN_TIMEOUT, node.client().deploy(app.clone()))
                .await
                .map_err(|_| anyhow::anyhow!("deploy to node {} timed out", node.n))??;
        }
        events.record(
            0,
            What::Note {
                text: format!("started; bound {bound:?}, advertised {advertised:?}"),
            },
        );
        let mut cluster = Self {
            nodes,
            faults,
            events,
            node_timeout: Duration::from_millis(args.node_timeout_ms),
            heartbeat: Duration::from_millis(args.heartbeat_ms),
            known: BTreeMap::new(),
        };
        cluster.until_connected().await;
        Ok(cluster)
    }

    fn node(&mut self, n: u32) -> &mut Node {
        &mut self.nodes[usize::try_from(n - 1).unwrap_or(0)]
    }

    async fn set_link(&self, from: u32, to: u32, mode: Mode) {
        self.faults.set(from, to, mode).await;
        self.events.record(
            from,
            What::Link {
                to,
                mode: mode.to_string(),
            },
        );
    }

    /// The instances on each node that answers `ps`.
    async fn refresh(&mut self) {
        for node in &self.nodes {
            if let Ok(Ok(ps)) = tokio::time::timeout(ADMIN_TIMEOUT, node.client().ps()).await {
                self.known.insert(node.n, ps);
            }
        }
    }

    /// Faults and operations for `length`, each fault undone after its
    /// time or at the end.
    async fn episode(&mut self, rng: &mut Rng, length: Duration) -> anyhow::Result<()> {
        let end = Instant::now() + length;
        let mut active: Vec<(Instant, Undo)> = Vec::new();
        let mut refreshed = Instant::now() - Duration::from_secs(60);
        while Instant::now() < end {
            let now = Instant::now();
            let (due, rest): (Vec<_>, Vec<_>) = active.into_iter().partition(|(at, _)| *at <= now);
            active = rest;
            for (_, undo) in due {
                self.undo(undo).await?;
            }
            if active.len() < FAULTS_MAX && rng.chance(80) {
                let busy: Vec<u32> = active
                    .iter()
                    .filter_map(|(_, u)| match u {
                        Undo::Thaw(n) | Undo::Restart(n) => Some(*n),
                        Undo::Link(..) => None,
                    })
                    .collect();
                active.extend(self.fault(rng, &busy).await?);
            }
            if refreshed.elapsed() > Duration::from_secs(2) {
                self.refresh().await;
                refreshed = Instant::now();
            }
            self.operate(rng).await;
            tokio::time::sleep(Duration::from_millis(rng.range(5, 100))).await;
        }
        for (_, undo) in active {
            self.undo(undo).await?;
        }
        Ok(())
    }

    /// One operation by a random instance, through its node's admin API.
    async fn operate(&mut self, rng: &mut Rng) {
        let all: Vec<(u32, String, InstanceInfo)> = self
            .known
            .iter()
            .flat_map(|(n, ps)| ps.iter().map(move |i| (*n, i.pid.clone(), i.clone())))
            .collect();
        if all.is_empty() {
            return;
        }
        let pick = |rng: &mut Rng| {
            all[usize::try_from(rng.range(0, all.len() as u64)).unwrap_or(0)].clone()
        };
        let (n, pid, info) = pick(rng);
        let op = *rng.pick(&[
            (8, Op::Link),
            (1, Op::Unlink),
            (6, Op::Monitor),
            (1, Op::Demonitor),
            (3, Op::Trap),
            (1, Op::KillInstance),
        ]);
        let command = match op {
            Op::Link => format!("link:{}", pick(rng).1),
            Op::Monitor => format!("monitor:{}", pick(rng).1),
            Op::Unlink => match info.links.first() {
                Some(other) => format!("unlink:{other}"),
                None => return,
            },
            Op::Demonitor => match info.monitoring.first() {
                Some(m) => format!("demonitor:{}", m.reference),
                None => return,
            },
            // Mostly trapping, so links outlive their partners' exits and
            // are there to check.
            Op::Trap => format!("trap-exits:{}", if rng.chance(800) { "on" } else { "off" }),
            Op::KillInstance => {
                let client = self.node(n).client();
                let _ = tokio::time::timeout(ADMIN_TIMEOUT, client.kill(&pid)).await;
                self.events.record(
                    n,
                    What::Note {
                        text: format!("killed instance {pid}"),
                    },
                );
                return;
            }
        };
        let client = self.node(n).client();
        // Failures are expected (the node is cut off, stopped or down, the
        // instance gone): the check after the heal is what counts.
        let _ = tokio::time::timeout(
            ADMIN_TIMEOUT,
            client.call_to(
                &Target::Pid(pid),
                "text/plain",
                command.into_bytes(),
                CALL_MS,
            ),
        )
        .await;
    }

    /// Start one fault; returns what ends it, and when.
    async fn fault(&mut self, rng: &mut Rng, busy: &[u32]) -> anyhow::Result<Vec<(Instant, Undo)>> {
        let window = u64::try_from(self.node_timeout.as_millis()).unwrap_or(1000);
        let now = Instant::now();
        let kind = *rng.pick(&[(7, Kind::Link), (1, Kind::Kill), (2, Kind::Freeze)]);
        let free: Vec<u32> = (1..=NODES).filter(|n| !busy.contains(n)).collect();
        match kind {
            Kind::Link => {
                let (x, y) = *rng.pick(&[(1, (1, 2)), (1, (1, 3)), (1, (2, 3))]);
                let directions = rng
                    .pick(&[
                        (1, vec![(x, y)]),
                        (1, vec![(y, x)]),
                        (2, vec![(x, y), (y, x)]),
                    ])
                    .clone();
                let mode = *rng.pick(&[(2, Mode::Cut), (1, Mode::Hold), (1, Mode::Drop(300))]);
                let until = now + Duration::from_millis(rng.range(200, 3 * window));
                let mut undo = Vec::new();
                for (from, to) in directions {
                    self.set_link(from, to, mode).await;
                    undo.push((until, Undo::Link(from, to)));
                }
                Ok(undo)
            }
            Kind::Kill if !free.is_empty() => {
                let n = free[usize::try_from(rng.range(0, free.len() as u64))?];
                self.node(n).kill();
                Ok(vec![(
                    now + Duration::from_millis(rng.range(0, 3000)),
                    Undo::Restart(n),
                )])
            }
            Kind::Freeze if !free.is_empty() => {
                let n = free[usize::try_from(rng.range(0, free.len() as u64))?];
                self.node(n).freeze().await?;
                let length = rng.range(100, 2 * window);
                Ok(vec![(now + Duration::from_millis(length), Undo::Thaw(n))])
            }
            Kind::Kill | Kind::Freeze => Ok(Vec::new()),
        }
    }

    async fn undo(&mut self, undo: Undo) -> anyhow::Result<()> {
        match undo {
            Undo::Link(from, to) => self.set_link(from, to, Mode::Pass).await,
            Undo::Thaw(n) => self.node(n).thaw()?,
            Undo::Restart(n) => {
                if !self.node(n).alive() {
                    self.node(n).spawn().await?;
                    self.redeploy(n).await;
                }
            }
        }
        Ok(())
    }

    /// A restarted node keeps its deployed apps in its data directory, so
    /// this only waits for the pool to be back.
    async fn redeploy(&mut self, n: u32) {
        let client = self.node(n).client();
        for _ in 0..100 {
            if let Ok(Ok(d)) = tokio::time::timeout(ADMIN_TIMEOUT, client.status(APP)).await
                && d.summary.status == "running"
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Every link passing, every node running and continued, and every node
    /// reaching every other.
    async fn heal(&mut self) -> anyhow::Result<()> {
        self.faults.heal_all().await;
        self.events.record(
            0,
            What::Note {
                text: "healed every link".into(),
            },
        );
        for n in 1..=NODES {
            self.node(n).thaw()?;
            if !self.node(n).alive() {
                self.node(n).spawn().await?;
                self.redeploy(n).await;
            }
        }
        self.until_connected().await;
        Ok(())
    }

    /// Wait until every node lists every other as reachable, then a little
    /// longer for frames in flight and sessions ending on either side.
    async fn until_connected(&mut self) {
        for _ in 0..200 {
            let mut all = true;
            for node in &self.nodes {
                let members = tokio::time::timeout(ADMIN_TIMEOUT, node.client().members()).await;
                let ok = matches!(&members, Ok(Ok(m))
                    if m.len() == NODES as usize && m.iter().all(|m| m.reachable));
                all &= ok;
            }
            if all {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(self.node_timeout + 4 * self.heartbeat).await;
    }

    /// Check that every node's `ps` agrees; retried for a few seconds, since
    /// a session ending late can still be clearing entries. Returns the
    /// problems of the last attempt.
    async fn agree(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        for _ in 0..10 {
            self.known.clear();
            self.refresh().await;
            problems = disagreements(&self.known);
            if self.known.len() < NODES as usize {
                problems.push(format!(
                    "only nodes {:?} answered ps",
                    self.known.keys().collect::<Vec<_>>()
                ));
            }
            if problems.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let entries: usize = self
            .known
            .values()
            .flatten()
            .map(|i| i.links.len() + i.monitoring.len())
            .sum();
        self.events.record(
            0,
            What::Note {
                text: format!(
                    "checked {entries} links and monitors: {} problems",
                    problems.len()
                ),
            },
        );
        problems
    }
}

/// Every link or monitor in `ps` (by node) without its counterpart: S12's
/// `Agreement`, plus that each entry names a live instance.
pub(crate) fn disagreements(ps: &BTreeMap<u32, Vec<InstanceInfo>>) -> Vec<String> {
    let by_pid: BTreeMap<&str, &InstanceInfo> =
        ps.values().flatten().map(|i| (i.pid.as_str(), i)).collect();
    let mut out = BTreeSet::new();
    for i in ps.values().flatten() {
        for other in &i.links {
            match by_pid.get(other.as_str()) {
                None => {
                    out.insert(format!("{} is linked to {other}, which is gone", i.pid));
                }
                Some(o) if !o.links.contains(&i.pid) => {
                    out.insert(format!(
                        "{} is linked to {other}, which does not list it",
                        i.pid
                    ));
                }
                Some(_) => {}
            }
        }
        for m in &i.monitoring {
            match by_pid.get(m.pid.as_str()) {
                None => {
                    out.insert(format!(
                        "{} monitors {} ({}), which is gone",
                        i.pid, m.pid, m.reference
                    ));
                }
                Some(t)
                    if !t
                        .monitored_by
                        .iter()
                        .any(|b| b.reference == m.reference && b.pid == i.pid) =>
                {
                    out.insert(format!(
                        "{} monitors {} ({}), which does not list it",
                        i.pid, m.pid, m.reference
                    ));
                }
                Some(_) => {}
            }
        }
        for m in &i.monitored_by {
            match by_pid.get(m.pid.as_str()) {
                None => {
                    out.insert(format!(
                        "{} is monitored by {} ({}), which is gone",
                        i.pid, m.pid, m.reference
                    ));
                }
                Some(w)
                    if !w
                        .monitoring
                        .iter()
                        .any(|b| b.reference == m.reference && b.pid == i.pid) =>
                {
                    out.insert(format!(
                        "{} is monitored by {} ({}), which does not list it",
                        i.pid, m.pid, m.reference
                    ));
                }
                Some(_) => {}
            }
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
#[path = "cluster_tests.rs"]
mod cluster_tests;
