//! The SCADA soak (ROADMAP X2, Phase 3's exit): the failover soak's
//! witness and pair, behind the same proxies and under the same faults,
//! running the SCADA example (`examples/scada`: `points`, `hmi` and the
//! `control` singleton in two versions) against two fake Modbus devices,
//! while a probe per node reads the point table and the alarms over HTTP
//! ten times a second.
//!
//! The run alternates episodes of faults with planned steps on the healed
//! pair: a release of `control` across the pair (`paddock release
//! upgrade`, then `release rollback` the next time), and a planned
//! switchover. Every other upgrade instead loses the primary between the
//! release's two steps (`--standby-only`, the primary killed, restarted,
//! and the release run again), which counts as a fault.
//!
//! It fails when:
//!
//! - a planned step on either node fails a point read, leaves a gap of
//!   [`GAP_MAX`] or more between point updates, or fails a request to
//!   `control` (alarms) other than during a switchover's hand-over, whose
//!   failures must all fall within [`HANDOVER_MAX`];
//! - after a heal, other than one node runs `control`, the acknowledged
//!   alarm is not `acked` on the primary, or a release does not bring both
//!   nodes to one version;
//! - the logs show a moment when both nodes ran `control` ([`check`]).
//!
//! Under faults, failed requests and point gaps are recorded, not judged.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::check;
use super::events::{EventLog, What};
use super::failover::{Args, Cluster, Setup, SetupApp};
use super::node::{ADMIN_TIMEOUT, bundle};
use super::rng::Rng;

/// The singleton.
const CONTROL: &str = "control";

/// The alarm raised and acknowledged before the first episode.
const ALARM: &str = "pump-1-pressure";

/// The longest gap between point updates a planned step may leave.
pub(crate) const GAP_MAX: Duration = Duration::from_secs(1);

/// How far apart the first and last failed request to `control` may be in
/// a switchover.
pub(crate) const HANDOVER_MAX: Duration = Duration::from_secs(1);

/// How long each planned step is watched after its command returns.
const OBSERVE: Duration = Duration::from_secs(3);

/// How long a probe request may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a release has to bring both nodes to one version.
const CONVERGE: Duration = Duration::from_secs(120);

/// What a probe asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ask {
    Points,
    Alarms,
}

/// One probe request: when it was sent (ms since the run started), to
/// which node, and how it ended; for a point read, the fake device's
/// counter (`pump-1/holding/0`) it showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sample {
    pub at_ms: u64,
    pub node: u32,
    pub ask: Ask,
    pub ok: bool,
    pub counter: Option<u64>,
}

/// What a window of the run was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A release or rollback across the healed pair.
    Release,
    /// A planned switchover.
    Switchover,
    /// An episode of faults, or the primary lost during a release.
    Fault,
}

/// A stretch of the run judged, or recorded, as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Window {
    pub name: String,
    pub kind: Kind,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// One node's probes over a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    pub points: u32,
    pub points_failed: u32,
    pub alarms: u32,
    pub alarms_failed: u32,
    /// The longest time between two reads that showed the counter change,
    /// while the node answered.
    pub gap_ms: u64,
    /// From the first to the last failed request to `control`.
    pub alarm_span_ms: u64,
}

/// Tally `node`'s samples (in the order sent) over `window`. A gap is
/// counted where it ends; the counter seen before the window starts it.
pub(crate) fn tally(samples: &[Sample], node: u32, window: &Window) -> Tally {
    let mut t = Tally::default();
    let mut changed: Option<(u64, u64)> = None;
    let mut first_failed = None;
    for s in samples.iter().filter(|s| s.node == node) {
        if s.at_ms >= window.end_ms {
            break;
        }
        let inside = s.at_ms >= window.start_ms;
        match s.ask {
            Ask::Alarms if inside => {
                t.alarms += 1;
                if !s.ok {
                    t.alarms_failed += 1;
                    let first = *first_failed.get_or_insert(s.at_ms);
                    t.alarm_span_ms = s.at_ms - first;
                }
            }
            Ask::Alarms => {}
            Ask::Points => {
                if inside {
                    t.points += 1;
                }
                if !s.ok {
                    if inside {
                        t.points_failed += 1;
                    }
                    changed = None;
                    continue;
                }
                let Some(counter) = s.counter else { continue };
                match changed {
                    Some((_, last)) if last == counter => {}
                    Some((at, _)) => {
                        if inside {
                            t.gap_ms = t.gap_ms.max(s.at_ms - at);
                        }
                        changed = Some((s.at_ms, counter));
                    }
                    None => changed = Some((s.at_ms, counter)),
                }
            }
        }
    }
    t
}

/// Why a planned window fails on one node, if it does.
pub(crate) fn judge(kind: Kind, t: &Tally) -> Vec<String> {
    let mut why = Vec::new();
    if kind == Kind::Fault {
        return why;
    }
    if t.points_failed > 0 {
        why.push(format!(
            "{} of {} point reads failed",
            t.points_failed, t.points
        ));
    }
    if t.gap_ms >= millis(GAP_MAX) {
        why.push(format!("a {} ms gap between point updates", t.gap_ms));
    }
    if t.points == 0 {
        why.push("no point reads".into());
    }
    match kind {
        Kind::Release if t.alarms_failed > 0 => {
            why.push(format!(
                "{} of {} alarm requests failed",
                t.alarms_failed, t.alarms
            ));
        }
        Kind::Switchover if t.alarm_span_ms > millis(HANDOVER_MAX) => why.push(format!(
            "alarm requests failed over {} ms ({} of {})",
            t.alarm_span_ms, t.alarms_failed, t.alarms
        )),
        _ => {}
    }
    why
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The fake device's counter in a `GET /scada/points` reply.
pub(crate) fn counter(body: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v["points"]["pump-1/holding/0"].as_u64()
}

/// The alarm's status in a `GET /scada/alarms` reply.
pub(crate) fn alarm_status(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v["alarms"][ALARM]["status"].as_str().map(String::from)
}

/// An HTTP/1.1 request to the node's ingress on `port`, with
/// `Connection: close`: the status and the body, or `None` for no answer
/// in time.
async fn http(port: u16, method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let exchange = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.ok()?;
        parse_reply(&String::from_utf8_lossy(&reply))
    };
    tokio::time::timeout(PROBE_TIMEOUT, exchange).await.ok()?
}

/// Status and body of an HTTP/1.1 reply, with a chunked body joined.
pub(crate) fn parse_reply(reply: &str) -> Option<(u16, String)> {
    let (head, body) = reply.split_once("\r\n\r\n")?;
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    let chunked = head.lines().any(|l| {
        l.to_ascii_lowercase()
            .starts_with("transfer-encoding: chunked")
    });
    if !chunked {
        return Some((status, body.to_string()));
    }
    let mut joined = String::new();
    let mut rest = body;
    loop {
        let (size, after) = rest.split_once("\r\n")?;
        let size = usize::from_str_radix(size.trim(), 16).ok()?;
        if size == 0 {
            return Some((status, joined));
        }
        joined.push_str(after.get(..size)?);
        rest = after.get(size + 2..)?;
    }
}

/// The fake devices' process, killed when dropped.
struct Devices(Child);

impl Drop for Devices {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Free TCP ports on loopback (another process could take one first; rare
/// enough in a soak run).
fn free_tcp(n: usize) -> anyhow::Result<Vec<u16>> {
    let held: Vec<_> = (0..n)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0"))
        .collect::<Result<_, _>>()?;
    held.iter().map(|l| Ok(l.local_addr()?.port())).collect()
}

/// The pair's config for node `n` (1 or 2): its HTTP ingress, the point
/// table as a `cluster` bucket, and the two devices, polled by the
/// primary alone.
fn pair_config(http: u16, devices: &[SocketAddr]) -> String {
    let mut c = format!(
        "[http.main]\naddress = \"127.0.0.1:{http}\"\nstandby = \"forward\"\n\
         [buckets.points]\ndurability = \"disk\"\nscope = \"cluster\"\n"
    );
    for (i, d) in devices.iter().enumerate() {
        let _ = write!(
            c,
            "[extensions.\"paddock-modbus:modbus\".devices.pump-{}]\naddress = \"{d}\"\n\
             poll_ms = 100\ntimeout_ms = 500\ndeliver_to = \"points\"\nprimary_only = true\n\
             holding = [{{ start = 0, count = 4 }}]\n",
            i + 1
        );
    }
    c
}

const POLICY: &str = r#"[apps.points]
grant = ["wasi:logging", "wasi:keyvalue"]
buckets = ["points"]
[apps.hmi]
grant = ["wasi:http/incoming-handler", "wasi:keyvalue", "wasi:logging"]
buckets = ["points"]
[apps.control]
grant = ["wasi:http/incoming-handler", "wasi:keyvalue", "wasi:logging", "wasi:config",
         "paddock-modbus:modbus"]
buckets = ["points"]
"#;

/// The probes' samples, and the run's clock.
struct Probes {
    start: Instant,
    samples: Mutex<Vec<Sample>>,
    stop: AtomicBool,
}

impl Probes {
    fn now_ms(&self) -> u64 {
        millis(self.start.elapsed())
    }

    fn snapshot(&self) -> Vec<Sample> {
        self.samples.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Read `path` from node `node` on `port` every 100 ms until stopped.
async fn probe(probes: Arc<Probes>, node: u32, port: u16, ask: Ask) {
    let path = match ask {
        Ask::Points => "/scada/points",
        Ask::Alarms => "/scada/alarms",
    };
    while !probes.stop.load(Ordering::Relaxed) {
        let at_ms = probes.now_ms();
        let reply = http(port, "GET", path, "").await;
        let ok = matches!(&reply, Some((200, _)));
        let counter = match (&reply, ask) {
            (Some((200, body)), Ask::Points) => counter(body),
            _ => None,
        };
        if let Ok(mut s) = probes.samples.lock() {
            s.push(Sample {
                at_ms,
                node,
                ask,
                ok,
                counter,
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Run, check, and return whether the run passed.
pub(crate) async fn run(args: &Args) -> anyhow::Result<bool> {
    let events = Arc::new(EventLog::create(&args.dir.join("events.jsonl"))?);
    let root = &args.root;
    let wasm = root.join("target/wasm32-wasip2/release");
    let ports = free_tcp(4)?;
    let (http_ports, device_ports) = ([ports[0], ports[1]], [ports[2], ports[3]]);
    let device_addrs: Vec<SocketAddr> = device_ports
        .iter()
        .map(|p| SocketAddr::from(([127, 0, 0, 1], *p)))
        .collect();
    let read = |p: &str| -> anyhow::Result<String> {
        std::fs::read_to_string(root.join("examples/scada").join(p))
            .with_context(|| format!("reading examples/scada/{p}"))
    };
    let setup = Setup {
        apps: vec![
            SetupApp {
                name: "points",
                manifest: read("points/paddock.toml")?,
                wasm: wasm.join("example_scada_points.wasm"),
            },
            SetupApp {
                name: "hmi",
                manifest: read("hmi/paddock.toml")?,
                wasm: wasm.join("example_scada_hmi.wasm"),
            },
            SetupApp {
                name: CONTROL,
                manifest: read("control/paddock.toml")?,
                wasm: wasm.join("example_scada_control.wasm"),
            },
        ],
        pair_config: [
            pair_config(http_ports[0], &device_addrs),
            pair_config(http_ports[1], &device_addrs),
        ],
        failover_config: "replication = true\n".into(),
        policy: Some(POLICY.into()),
    };
    let fake = root.join("target/release/examples/fake_device");
    let devices = Devices(
        Command::new(&fake)
            .args(device_addrs.iter().map(ToString::to_string))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", fake.display()))?,
    );
    // Version 2, signed by the key the cluster's bundles use.
    let mut cluster = Cluster::start(args, &setup, events.clone()).await?;
    let v2 = root
        .join("target/fixtures/scada-drill-v2/wasm32-wasip2/release/example_scada_control.wasm");
    bundle(
        &args.paddock,
        &args.dir,
        "control-v2",
        &read("control/paddock-v2.toml")?,
        &v2,
    )?;
    std::fs::write(
        args.dir.join("pair.toml"),
        format!(
            "[[node]]\nid = 1\nsocket = {:?}\n\n[[node]]\nid = 2\nsocket = {:?}\n",
            cluster.nodes[0].socket(),
            cluster.nodes[1].socket()
        ),
    )?;
    let mut soak = Soak {
        args,
        events: events.clone(),
        http: http_ports,
        probes: Arc::new(Probes {
            start: Instant::now(),
            samples: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        }),
        windows: Vec::new(),
        problems: Vec::new(),
        at_v2: false,
    };
    let outcome = soak.cycle(&mut cluster).await;
    soak.probes.stop.store(true, Ordering::Relaxed);
    drop(cluster);
    drop(devices);
    outcome?;
    soak.report()
}

/// The run's state.
struct Soak<'a> {
    args: &'a Args,
    events: Arc<EventLog>,
    http: [u16; 2],
    probes: Arc<Probes>,
    windows: Vec<Window>,
    problems: Vec<String>,
    /// Whether the last release went to version 2.
    at_v2: bool,
}

impl Soak<'_> {
    fn note(&self, text: String) {
        self.events.record(0, What::Note { text });
    }

    fn problem(&mut self, text: String) {
        self.note(format!("PROBLEM: {text}"));
        self.problems.push(text);
    }

    /// Setup, then episodes and planned steps until the deadline or a
    /// problem.
    async fn cycle(&mut self, cluster: &mut Cluster) -> anyhow::Result<()> {
        let args = self.args;
        let deadline = Instant::now() + Duration::from_secs_f64(args.minutes * 60.0);
        let mut rng = Rng::new(args.seed);
        if !cluster.settle(CONTROL).await {
            self.problem("control did not start on exactly one node".into());
            return Ok(());
        }
        self.raise_alarm().await?;
        for (n, &port) in (1..=2).zip(&self.http) {
            for ask in [Ask::Points, Ask::Alarms] {
                tokio::spawn(probe(self.probes.clone(), n, port, ask));
            }
        }
        tokio::time::sleep(OBSERVE).await;
        let mut round = 0_u32;
        while Instant::now() < deadline && self.problems.is_empty() {
            round += 1;
            let left = deadline.saturating_duration_since(Instant::now());
            let start = self.probes.now_ms();
            cluster.episode(&mut rng, args.episode.min(left)).await?;
            cluster.heal().await?;
            let healthy = cluster.settle(CONTROL).await;
            self.windows.push(Window {
                name: format!("round {round}: faults"),
                kind: Kind::Fault,
                start_ms: start,
                end_ms: self.probes.now_ms(),
            });
            if !healthy {
                self.problem(format!(
                    "round {round}: control not on exactly one node after the heal"
                ));
                break;
            }
            self.converge(cluster, round).await?;
            self.check_alarm(round).await;
            if !self.problems.is_empty() || Instant::now() >= deadline {
                break;
            }
            // Every other upgrade loses the primary between its steps.
            if !self.at_v2 && round % 4 == 3 {
                self.release_primary_lost(cluster, round).await?;
            } else {
                self.release(round).await?;
            }
            self.switchover(cluster, round).await?;
            self.check_alarm(round).await;
        }
        Ok(())
    }

    /// Write the pump's pressure past its limit, then acknowledge the
    /// alarm it raises.
    async fn raise_alarm(&mut self) -> anyhow::Result<()> {
        let port = self.http[0];
        let write = r#"{"device":"pump-1","table":"holding","address":1,"value":200}"#;
        for _ in 0..100 {
            if matches!(
                http(port, "POST", "/scada/write", write).await,
                Some((200, _))
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        for _ in 0..100 {
            let status = http(port, "GET", "/scada/alarms", "").await;
            if status.is_some_and(|(_, b)| alarm_status(&b).as_deref() == Some("active")) {
                let path = format!("/scada/alarms/{ALARM}/ack");
                if matches!(http(port, "POST", &path, "").await, Some((200, _))) {
                    self.note(format!("{ALARM} raised and acknowledged"));
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        anyhow::bail!("{ALARM} was not raised and acknowledged")
    }

    /// The primary's node number, from the admin API.
    async fn primary(cluster: &Cluster) -> Option<u32> {
        for node in &cluster.nodes[..2] {
            let f = tokio::time::timeout(ADMIN_TIMEOUT, node.client().failover()).await;
            if matches!(f, Ok(Ok(f)) if f.role == "primary" && f.may_run) {
                return Some(node.n);
            }
        }
        None
    }

    /// After a heal, the alarm as the primary's `control` sees it.
    async fn check_alarm(&mut self, round: u32) {
        let mut seen = None;
        for _ in 0..20 {
            for port in self.http {
                if let Some((200, body)) = http(port, "GET", "/scada/alarms", "").await {
                    seen = alarm_status(&body);
                    if seen.as_deref() == Some("acked") {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.problem(format!("round {round}: {ALARM} is {seen:?}, not acked"));
    }

    /// `paddock release <args> --pair pair.toml --json`, and its outcome.
    async fn release_cmd(&self, release: &[&str]) -> anyhow::Result<(String, serde_json::Value)> {
        let paddock = self.args.paddock.clone();
        let dir = self.args.dir.clone();
        let words: Vec<String> = release.iter().map(|s| (*s).to_string()).collect();
        let out = tokio::task::spawn_blocking(move || {
            Command::new(&paddock)
                .arg("--json")
                .arg("release")
                .args(&words)
                .arg("--pair")
                .arg(dir.join("pair.toml"))
                .current_dir(&dir)
                .output()
        })
        .await??;
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
            serde_json::json!({
                "outcome": "error",
                "reason": String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        });
        let outcome = report["outcome"].as_str().unwrap_or("error").to_string();
        self.note(format!("release {}: {report}", release.join(" ")));
        Ok((outcome, report))
    }

    /// The release that moves `control` to the version the cycle is at.
    fn toward(&self) -> Vec<&'static str> {
        if self.at_v2 {
            vec!["upgrade", CONTROL, "control-v2.rbp"]
        } else {
            vec!["rollback", CONTROL]
        }
    }

    /// Bring both nodes to the cycle's version, rerunning the release
    /// while they are not (a run stopped by a node catching up, roles
    /// changing).
    async fn converge(&mut self, cluster: &Cluster, round: u32) -> anyhow::Result<()> {
        let target = if self.at_v2 { "2.0.0" } else { "1.0.0" };
        let end = Instant::now() + CONVERGE;
        let mut last = String::new();
        while Instant::now() < end {
            let mut versions = Vec::new();
            for node in &cluster.nodes[..2] {
                let d = tokio::time::timeout(ADMIN_TIMEOUT, node.client().status(CONTROL)).await;
                versions.push(match d {
                    Ok(Ok(d)) => d.summary.version,
                    _ => String::new(),
                });
            }
            if versions.iter().all(|v| v == target) {
                return Ok(());
            }
            let (_, report) = self.release_cmd(&self.toward()).await?;
            last = format!("{versions:?}; {report}");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        self.problem(format!(
            "round {round}: the release did not converge: {last}"
        ));
        Ok(())
    }

    /// End a window that started at `start_ms`.
    fn close(&mut self, name: String, kind: Kind, start_ms: u64) {
        self.windows.push(Window {
            name,
            kind,
            start_ms,
            end_ms: self.probes.now_ms(),
        });
    }

    /// Release `control` to the other version across the pair.
    async fn release(&mut self, round: u32) -> anyhow::Result<()> {
        self.at_v2 = !self.at_v2;
        let start = self.probes.now_ms();
        let (outcome, report) = self.release_cmd(&self.toward()).await?;
        tokio::time::sleep(OBSERVE).await;
        let name = format!(
            "round {round}: release {}",
            if self.at_v2 { "upgrade" } else { "rollback" }
        );
        self.close(name, Kind::Release, start);
        if outcome != "done" {
            self.problem(format!(
                "round {round}: release on the healed pair: {report}"
            ));
        }
        Ok(())
    }

    /// Upgrade the standby, kill the primary, restart it, and finish the
    /// release on the new roles.
    async fn release_primary_lost(
        &mut self,
        cluster: &mut Cluster,
        round: u32,
    ) -> anyhow::Result<()> {
        self.at_v2 = true;
        let start = self.probes.now_ms();
        let mut step = self.toward();
        step.push("--standby-only");
        let (outcome, report) = self.release_cmd(&step).await?;
        if outcome != "paused" {
            self.problem(format!("round {round}: release --standby-only: {report}"));
        }
        if let Some(n) = Self::primary(cluster).await {
            cluster.node(n).kill();
            tokio::time::sleep(self.args.timings.settle()).await;
            cluster.node(n).spawn().await?;
        }
        if !cluster.settle(CONTROL).await {
            self.problem(format!(
                "round {round}: control not on exactly one node after losing the primary"
            ));
        }
        self.converge(cluster, round).await?;
        tokio::time::sleep(OBSERVE).await;
        self.close(
            format!("round {round}: release upgrade, primary lost"),
            Kind::Fault,
            start,
        );
        Ok(())
    }

    /// A planned switchover from whichever node is primary.
    async fn switchover(&mut self, cluster: &mut Cluster, round: u32) -> anyhow::Result<()> {
        let Some(from) = Self::primary(cluster).await else {
            self.problem(format!("round {round}: no primary for the switchover"));
            return Ok(());
        };
        // The standby must hold every replicated write first.
        for _ in 0..120 {
            let f = cluster.nodes[usize::try_from(from - 1)?]
                .client()
                .failover()
                .await;
            if f.is_ok_and(|f| !f.partner_outdated) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let start = self.probes.now_ms();
        let client = cluster.nodes[usize::try_from(from - 1)?].client();
        let result = tokio::time::timeout(ADMIN_TIMEOUT, client.switchover()).await;
        let to = 3 - from;
        let mut moved = false;
        for _ in 0..100 {
            if Self::primary(cluster).await == Some(to) {
                moved = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(OBSERVE).await;
        self.close(
            format!("round {round}: switchover {from} -> {to}"),
            Kind::Switchover,
            start,
        );
        if !moved {
            self.problem(format!(
                "round {round}: switchover {from} -> {to} did not happen: {result:?}"
            ));
        }
        Ok(())
    }

    /// Judge the planned windows, record the rest, write `windows.txt` and
    /// `result.txt`, and return whether the run passed.
    fn report(&mut self) -> anyhow::Result<bool> {
        let samples = self.probes.snapshot();
        let mut lines = String::from(
            "window; kind; per node: point reads failed/made, alarm requests failed/made, \
             longest point gap ms, span of failed alarm requests ms\n",
        );
        let mut worst_fault_gap = 0;
        let (mut fault_failed, mut fault_made) = (0, 0);
        let mut planned = 0;
        for w in &self.windows.clone() {
            let mut line = format!("{}; {:?}", w.name, w.kind);
            for node in 1..=2 {
                let t = tally(&samples, node, w);
                let _ = write!(
                    line,
                    "; node {node}: {}/{} {}/{} {} {}",
                    t.points_failed, t.points, t.alarms_failed, t.alarms, t.gap_ms, t.alarm_span_ms
                );
                if w.kind == Kind::Fault {
                    worst_fault_gap = worst_fault_gap.max(t.gap_ms);
                    fault_failed += t.points_failed + t.alarms_failed;
                    fault_made += t.points + t.alarms;
                }
                for why in judge(w.kind, &t) {
                    self.problems
                        .push(format!("{}, node {node}: {why}", w.name));
                }
            }
            if w.kind != Kind::Fault {
                planned += 1;
            }
            lines += &line;
            lines.push('\n');
        }
        std::fs::write(self.args.dir.join("windows.txt"), &lines)?;
        let both = check::check_dir(&self.args.dir, CONTROL)?;
        let mut text = format!(
            "{} windows, {planned} planned; under faults {fault_failed} of {fault_made} requests failed, \
             longest point gap {worst_fault_gap} ms\n",
            self.windows.len()
        );
        text += &both.render();
        if planned == 0 {
            self.problems.push("no planned step ran".into());
        }
        for p in &self.problems {
            let _ = writeln!(text, "PROBLEM: {p}");
        }
        std::fs::write(self.args.dir.join("result.txt"), &text)?;
        print!("{text}");
        Ok(self.problems.is_empty() && both.passed())
    }
}

#[cfg(test)]
#[path = "scada_tests.rs"]
mod scada_tests;
