//! The failover soak: a witness (node 3) and a pair (nodes 1 and 2) as
//! `paddock node` processes, each behind a proxy, running the counter
//! example as a singleton.
//!
//! The run is a series of episodes. In each, faults start at random times
//! from the seed, at most two at once, each undone after a random time:
//! a direction or both between two nodes cut, held or lossy; a node killed
//! with `SIGKILL` and restarted; a node stopped with `SIGSTOP` and
//! continued, sometimes for longer than its lease; with `--serial`, the
//! pair's serial line cut (alone, it changes nothing; with the network
//! between the pair cut too, the lease keeps at most one runner). After
//! each episode
//! everything is healed, and once the pair has had time to settle exactly
//! one node must run the singleton. At the end [`super::check`]
//! reads the logs for any moment when both ran it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::check;
use super::events::{EventLog, What};
use super::node::{ADMIN_TIMEOUT, Node, bundle, free_udp, socket_path};
use super::proxy::{self, Faults, Mode};
use super::rng::Rng;
use paddock::serial::{Tty, pty, relay};
use tokio::sync::watch;

/// The singleton's app name, from `examples/counter/paddock.toml`.
pub(crate) const APP: &str = "counter";

/// At most this many faults at once.
const FAULTS_MAX: usize = 2;

/// The pair's timings, as in `examples/failover/demo.sh`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timings {
    pub heartbeat_ms: u64,
    pub node_timeout_ms: u64,
    pub lease_ms: u64,
    pub margin_ms: u64,
}

impl Timings {
    /// Time for the pair to hand over after a fault ends, with slack.
    pub(crate) fn settle(&self) -> Duration {
        Duration::from_millis(self.node_timeout_ms + 2 * self.lease_ms + 2 * self.margin_ms + 2000)
    }
}

/// A failover run.
#[derive(Debug)]
pub(crate) struct Args {
    pub dir: PathBuf,
    pub root: PathBuf,
    pub paddock: PathBuf,
    pub seed: u64,
    pub minutes: f64,
    pub episode: Duration,
    pub timings: Timings,
    pub serial: Option<SerialSpec>,
}

/// The pair's serial line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SerialSpec {
    /// Two ptys joined by the runner.
    Pty,
    /// Two devices joined by a cable: node 1 through a pty relayed to the
    /// first, node 2 on the second.
    Devices(PathBuf, PathBuf),
}

impl SerialSpec {
    pub(crate) fn parse(text: &str) -> anyhow::Result<Self> {
        match text.split_once(',') {
            None if text == "pty" => Ok(Self::Pty),
            Some((a, b)) if !a.is_empty() && !b.is_empty() => Ok(Self::Devices(a.into(), b.into())),
            _ => anyhow::bail!("--serial takes `pty` or `DEV_A,DEV_B`, not {text:?}"),
        }
    }
}

/// The pair's serial line as the runner holds it: the devices nodes 1 and
/// 2 open, the switch that cuts it, and the relay.
#[derive(Debug)]
struct SerialLine {
    devices: [PathBuf; 2],
    cut: watch::Sender<bool>,
    relay: tokio::task::JoinHandle<()>,
}

impl SerialLine {
    fn start(spec: &SerialSpec) -> anyhow::Result<Self> {
        let (one, device1) = pty()?;
        let (two, device2) = match spec {
            SerialSpec::Pty => pty()?,
            SerialSpec::Devices(a, b) => (Tty::open(a, BAUD)?, b.clone()),
        };
        let (cut, cut_rx) = watch::channel(false);
        Ok(Self {
            devices: [device1, device2],
            cut,
            relay: tokio::spawn(relay(one, two, cut_rx)),
        })
    }
}

impl Drop for SerialLine {
    fn drop(&mut self) {
        self.relay.abort();
    }
}

/// The serial line's speed.
const BAUD: u32 = 115_200;

/// What the cluster runs: the app deployed to the pair, and what the
/// pair's node configs and policy add.
#[derive(Debug)]
pub(crate) struct Setup {
    /// The app's name, and its manifest and component.
    pub app: &'static str,
    pub manifest: String,
    pub wasm: PathBuf,
    /// Added to each pair node's config, before its `[cluster]` table.
    pub pair_config: String,
    /// The pair's operator policy, if the app needs grants.
    pub policy: Option<String>,
}

/// The counter example as a singleton.
fn singleton(args: &Args) -> anyhow::Result<Setup> {
    let manifest = std::fs::read_to_string(args.root.join("examples/counter/paddock.toml"))?
        + "\n[appserver.placement]\nmode = \"singleton\"\n";
    Ok(Setup {
        app: APP,
        manifest,
        wasm: args
            .root
            .join("target/wasm32-wasip2/release/example_counter.wasm"),
        pair_config: String::new(),
        policy: None,
    })
}

/// Run, check, and return whether the run passed.
pub(crate) async fn run(args: &Args) -> anyhow::Result<bool> {
    let events = Arc::new(EventLog::create(&args.dir.join("events.jsonl"))?);
    let deadline = Instant::now() + Duration::from_secs_f64(args.minutes * 60.0);
    let mut rng = Rng::new(args.seed);
    let outcome = {
        let mut cluster = Cluster::start(args, &singleton(args)?, events.clone()).await?;
        let mut ok = cluster.settle().await;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            cluster.episode(&mut rng, args.episode.min(left)).await?;
            cluster.heal().await?;
            ok &= cluster.settle().await;
        }
        ok
        // Dropping the cluster kills every node, recorded.
    };
    let report = check::check_dir(&args.dir, APP)?;
    let mut text = report.render();
    if !outcome && report.unsettled.is_empty() {
        let _ = writeln!(text, "a heal ended without exactly one runner");
    }
    std::fs::write(args.dir.join("result.txt"), &text)?;
    print!("{text}");
    Ok(report.passed() && outcome)
}

/// What ends a fault.
#[derive(Debug, Clone, Copy)]
enum Undo {
    Link(u32, u32),
    Thaw(u32),
    Restart(u32),
    Serial,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Link,
    Kill,
    Freeze,
    Serial,
}

/// The three nodes, their proxies and the faults between them.
#[derive(Debug)]
pub(crate) struct Cluster {
    pub(crate) nodes: Vec<Node>,
    faults: Arc<Faults>,
    events: Arc<EventLog>,
    timings: Timings,
    serial: Option<SerialLine>,
}

impl Cluster {
    /// Keys, configs, proxies and the bundle; start the witness and the
    /// pair; deploy the app to the pair.
    pub(crate) async fn start(
        args: &Args,
        setup: &Setup,
        events: Arc<EventLog>,
    ) -> anyhow::Result<Self> {
        let dir = &args.dir;
        let t = args.timings;
        let mut peers_file = String::from("[peers]\n");
        for n in 1..=3 {
            let public = super::node::paddock(
                &args.paddock,
                dir,
                &["cluster", "keygen", &format!("node-{n}.key")],
            )?;
            let _ = writeln!(peers_file, "{n} = [\"{}\"]", public.trim());
        }
        std::fs::write(dir.join("peers.toml"), peers_file)?;

        let (app, signer) = bundle(&args.paddock, dir, setup.app, &setup.manifest, &setup.wasm)?;
        if let Some(policy) = &setup.policy {
            std::fs::write(dir.join("policy.toml"), policy)?;
        }

        let bound: Vec<_> = (0..3).map(|_| free_udp()).collect::<Result<_, _>>()?;
        let names: HashMap<_, _> = bound.iter().copied().zip(1..=3).collect();
        let faults = Arc::new(Faults::new(args.seed));
        let mut advertised = Vec::new();
        for (n, &address) in (1..=3).zip(&bound) {
            advertised.push(proxy::start(n, address, names.clone(), faults.clone()).await?);
        }

        let serial = args.serial.as_ref().map(SerialLine::start).transpose()?;
        let mut nodes = Vec::new();
        for n in 1..=3_u32 {
            let i = usize::try_from(n - 1)?;
            let node = Node::new(n, &args.paddock, dir, socket_path("f", n), events.clone());
            let mut config = format!(
                "node = {n}\ndata_dir = {:?}\nadmin_socket = {:?}\ntrusted_keys = [{signer:?}]\n\
                 max_instances = 32\n[cluster]\naddress = \"{}\"\nadvertise = \"{}\"\n\
                 key = {:?}\npeers = {:?}\nheartbeat_interval_ms = {}\nnode_timeout_ms = {}\n",
                dir.join(format!("data-{n}")),
                node.socket(),
                bound[i],
                advertised[i],
                dir.join(format!("node-{n}.key")),
                dir.join("peers.toml"),
                t.heartbeat_ms,
                t.node_timeout_ms,
            );
            if n != 3 {
                // The policy is a root key and the extra tables follow the
                // root keys: both go before `[cluster]`.
                let mut extra = String::new();
                if setup.policy.is_some() {
                    let _ = writeln!(extra, "policy = {:?}", dir.join("policy.toml"));
                }
                extra += &setup.pair_config;
                config = config.replacen("[cluster]\n", &format!("{extra}[cluster]\n"), 1);
            }
            if n == 3 {
                config += "role = \"witness\"\n[witness]\npair = [1, 2]\nfirst = 1\n";
            } else {
                let _ = write!(
                    config,
                    "seeds = [\"{}\"]\n[failover]\npartner = {}\narbiter = \"witness\"\nwitness = 3\n",
                    advertised[2],
                    3 - n
                );
                if let Some(line) = &serial {
                    let _ = writeln!(
                        config,
                        "serial = {{ device = {:?}, baud = {BAUD} }}",
                        line.devices[i]
                    );
                }
            }
            let _ = writeln!(
                config,
                "lease_ms = {}\nmargin_ms = {}",
                t.lease_ms, t.margin_ms
            );
            std::fs::write(node.config_path(), config)?;
            nodes.push(node);
        }
        for i in [2, 0, 1] {
            nodes[i].spawn().await?;
        }
        for node in &nodes[..2] {
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
        Ok(Self {
            nodes,
            faults,
            events,
            timings: t,
            serial,
        })
    }

    fn set_serial(&self, cut: bool) {
        if let Some(line) = &self.serial {
            line.cut.send_replace(cut);
            self.events.record(0, What::Serial { cut });
        }
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

    /// Faults for `length`, each undone after its time or at the end.
    pub(crate) async fn episode(&mut self, rng: &mut Rng, length: Duration) -> anyhow::Result<()> {
        let end = Instant::now() + length;
        let mut active: Vec<(Instant, Undo)> = Vec::new();
        while Instant::now() < end {
            let now = Instant::now();
            let (due, rest): (Vec<_>, Vec<_>) = active.into_iter().partition(|(at, _)| *at <= now);
            active = rest;
            for (_, undo) in due {
                self.undo(undo).await?;
            }
            if active.len() < FAULTS_MAX && rng.chance(600) {
                let busy: Vec<u32> = active
                    .iter()
                    .filter_map(|(_, u)| match u {
                        Undo::Thaw(n) | Undo::Restart(n) => Some(*n),
                        Undo::Link(..) | Undo::Serial => None,
                    })
                    .collect();
                let serial_cut = active.iter().any(|(_, u)| matches!(u, Undo::Serial));
                active.extend(self.fault(rng, &busy, serial_cut).await?);
            }
            tokio::time::sleep(Duration::from_millis(rng.range(100, 1000))).await;
        }
        Ok(())
    }

    /// Start one fault; returns what ends it, and when.
    async fn fault(
        &mut self,
        rng: &mut Rng,
        busy: &[u32],
        serial_cut: bool,
    ) -> anyhow::Result<Vec<(Instant, Undo)>> {
        let t = self.timings;
        let window = t.node_timeout_ms + t.lease_ms + t.margin_ms;
        let now = Instant::now();
        let serial_weight = if self.serial.is_some() && !serial_cut {
            3
        } else {
            0
        };
        let kind = *rng.pick(&[
            (5, Kind::Link),
            (2, Kind::Kill),
            (3, Kind::Freeze),
            (serial_weight, Kind::Serial),
        ]);
        let free: Vec<u32> = (1..=3).filter(|n| !busy.contains(n)).collect();
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
                // Up to twice the lease: long pauses outlast it.
                let length = rng.range(100, 2 * t.lease_ms + t.margin_ms);
                Ok(vec![(now + Duration::from_millis(length), Undo::Thaw(n))])
            }
            Kind::Serial => {
                self.set_serial(true);
                let until = now + Duration::from_millis(rng.range(200, 3 * window));
                Ok(vec![(until, Undo::Serial)])
            }
            Kind::Kill | Kind::Freeze => Ok(Vec::new()),
        }
    }

    async fn undo(&mut self, undo: Undo) -> anyhow::Result<()> {
        match undo {
            Undo::Link(from, to) => self.set_link(from, to, Mode::Pass).await,
            Undo::Serial => self.set_serial(false),
            Undo::Thaw(n) => self.node(n).thaw()?,
            Undo::Restart(n) => {
                if !self.node(n).alive() {
                    self.node(n).spawn().await?;
                }
            }
        }
        Ok(())
    }

    /// Every link passing, every node running and continued.
    pub(crate) async fn heal(&mut self) -> anyhow::Result<()> {
        self.faults.heal_all().await;
        if self.serial.as_ref().is_some_and(|l| *l.cut.borrow()) {
            self.set_serial(false);
        }
        self.events.record(
            0,
            What::Note {
                text: "healed every link".into(),
            },
        );
        for node in &mut self.nodes {
            node.thaw()?;
            if !node.alive() {
                node.spawn().await?;
            }
        }
        Ok(())
    }

    /// Wait for the pair to settle, then record which nodes run the
    /// singleton; true if exactly one does.
    async fn settle(&mut self) -> bool {
        tokio::time::sleep(self.timings.settle()).await;
        let mut runners = Vec::new();
        for _ in 0..30 {
            runners.clear();
            for node in &self.nodes[..2] {
                let status = tokio::time::timeout(ADMIN_TIMEOUT, node.client().status(APP)).await;
                if matches!(status, Ok(Ok(d)) if d.summary.status == "running") {
                    runners.push(node.n);
                }
            }
            if runners.len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.events.record(
            0,
            What::Settled {
                runners: runners.clone(),
            },
        );
        runners.len() == 1
    }
}
