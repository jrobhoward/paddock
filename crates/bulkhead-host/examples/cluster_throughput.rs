//! Throughput between nodes, for `docs/BASELINE.md`: two
//! runtimes joined over the QUIC transport, with mutual TLS and pinned keys
//! as `bulkhead node` uses. The host on node 1 calls and casts instances of
//! `bulkhead-fixture-msgbench` on node 2, and instances on node 1 call and
//! cast them as apps do; the same on node 1 alone for comparison. Both
//! nodes run in one process over loopback, as two processes on one
//! machine, or on two machines.
//!
//! The fixture is one source built as a component and as a native program;
//! `CLUSTER_NATIVE=1` runs the native program (Linux), through the
//! `bulkhead-runner` in `target/release`, on the node it is set for. Set it
//! the same on both nodes.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p bulkhead-fixture-msgbench \
//!     --target-dir target/fixtures
//! cargo build --release -p bulkhead-native --bin bulkhead-runner
//! cargo build --release -p bulkhead-fixture-msgbench --features native \
//!     --bin bulkhead-fixture-msgbench
//! cargo run --release -p bulkhead-host --example cluster_throughput -- 4
//! # Two processes or two machines: keys for both nodes, copied to both.
//! cargo run --release -p bulkhead-host --example cluster_throughput -- 4 keys DIR
//! # Node 2 at its own address; it writes DIR/pids.
//! cargo run --release -p bulkhead-host --example cluster_throughput -- 4 serve DIR ADDR:7400
//! # Node 1, with node 2's DIR/pids in its DIR (copied, on another machine).
//! cargo run --release -p bulkhead-host --example cluster_throughput -- 4 connect DIR ADDR:7401 NODE2:7400
//! # On one machine: serve at 127.0.0.1:7400, connect at 127.0.0.1:7401.
//! ```
//!
//! Arguments: tokio worker threads (default 4), shared by both nodes in one
//! process; then nothing, `keys DIR` (write `node-1.key`, `node-2.key` and
//! `peers.toml`), `serve DIR ADDR:PORT` (node 2 with its instances, until
//! killed), or `connect DIR ADDR:PORT SEED:PORT` (node 1, joining node 2 at
//! the seed, then the runs below). Each node binds and advertises its own
//! address, so it must be one the other node reaches.
//!
//! From node 1's host:
//! - **Sequential calls:** one caller, `echo:x`, one at a time: latency.
//! - **Concurrent calls:** 16 and 64 callers over 16 instances: calls per
//!   second.
//! - **Casts:** `count` casts to 16 instances, then a call to each to read
//!   its count: casts handled per second, and how many were refused.
//! - **Payloads:** `mirror` calls with 1 KiB and 64 KiB bodies, 16 callers:
//!   MiB per second each way.
//!
//! From node 1's instances, timed by the apps' own clocks:
//! - **Sequential calls:** one instance calling another one at a time.
//! - **Concurrent calls:** 16 instances each calling its own target (on
//!   one node, 16 more instances that only answer).
//! - **Casts:** 16 instances each casting `count` to its own target, then
//!   calling it behind them; then the targets' counts read: casts handled
//!   per second.
//! - **Sends by kind** (native programs only): the same with `cast`,
//!   `try_cast` and `cast_timeout`: casts handled per second, how many
//!   sends returned `Ok`, and how many arrived. Every `Ok` of
//!   `cast_timeout` must have arrived; the run says so if one did not.
//!
//! `CLUSTER_MAILBOX=<n>` gives every instance a mailbox of `n` messages
//! (65,536 by default), on the node it is set for, so casts outrun their
//! targets: set it the same on both nodes.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::transport::quic::{
    NodeIdentity, QuicConfig, QuicTransport, SEND_BUFFER_BYTES, generate_key,
};
use bulkhead_host::{
    CallError, Envelope, InstanceHandle, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Pid,
    Runtime, RuntimeConfig,
};

const LONG: Duration = Duration::from_secs(30);
const SEQUENTIAL: usize = 20_000;
const TIMED: Duration = Duration::from_secs(5);
const INSTANCES: usize = 16;
const CASTS: u64 = 400_000;
/// Calls each instance makes in the runs from instances.
const APP_CALLS: u64 = 5_000;
/// Casts each instance sends in the runs from instances.
const APP_CASTS: u64 = 25_000;

/// What this process runs.
enum Mode {
    /// Both nodes, over loopback.
    Local,
    Keys(PathBuf),
    /// Node 2 at this address.
    Serve(PathBuf, SocketAddr),
    /// Node 1 at the first address, joining node 2 at the second.
    Connect(PathBuf, SocketAddr, SocketAddr),
}

fn main() {
    let mut args = std::env::args().skip(1);
    let workers: usize = args.next().map_or(4, |a| a.parse().expect("workers"));
    let rest: Vec<String> = args.collect();
    let mode = match rest.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => Mode::Local,
        ["keys", dir] => Mode::Keys(dir.into()),
        ["serve", dir, bind] => Mode::Serve(dir.into(), resolve(bind)),
        ["connect", dir, bind, seed] => Mode::Connect(dir.into(), resolve(bind), resolve(seed)),
        _ => panic!(
            "usage: cluster_throughput [WORKERS] [keys DIR | serve DIR ADDR:PORT | \
             connect DIR ADDR:PORT SEED:PORT]"
        ),
    };
    if let Mode::Keys(dir) = &mode {
        std::fs::create_dir_all(dir).unwrap();
        write_keys(dir);
        return;
    }
    let code = Code::find();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            tokio::spawn(async move {
                match mode {
                    Mode::Local => local(workers, &code).await,
                    Mode::Serve(dir, bind) => serve(&dir, bind, &code).await,
                    Mode::Connect(dir, bind, seed) => {
                        connect(workers, &dir, bind, seed, &code).await;
                    }
                    Mode::Keys(_) => unreachable!(),
                }
            })
            .await
        })
        .unwrap();
}

const TARGET: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");

/// The fixture: its component, or with `CLUSTER_NATIVE=1` its native
/// program.
enum Code {
    Component(Vec<u8>),
    Native(PathBuf),
}

impl Code {
    fn find() -> Self {
        if std::env::var_os("CLUSTER_NATIVE").is_some_and(|v| v == "1") {
            let path = Path::new(TARGET).join("release/bulkhead-fixture-msgbench");
            assert!(path.exists(), "{}: build it first", path.display());
            return Self::Native(path);
        }
        let path =
            format!("{TARGET}/fixtures/wasm32-wasip2/release/bulkhead_fixture_msgbench.wasm");
        Self::Component(std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
    }

    fn load(&self, rt: &Runtime) -> LoadedApp {
        match self {
            Self::Component(bytes) => rt.load(bytes).unwrap(),
            Self::Native(path) => rt.load_native(NativeCode {
                path: path.clone(),
                kind: NativeProgram::Executable,
            }),
        }
    }

    fn kind(&self) -> String {
        let kind = match self {
            Self::Component(_) => "components",
            Self::Native(_) => "native programs",
        };
        format!("{kind}, mailboxes of {}", mailbox_max())
    }

    fn native(&self) -> bool {
        matches!(self, Self::Native(_))
    }
}

/// The first IPv4 address of `HOST:PORT`, else the first.
fn resolve(address: &str) -> SocketAddr {
    let all: Vec<SocketAddr> = address.to_socket_addrs().expect("HOST:PORT").collect();
    *all.iter()
        .find(|a| a.is_ipv4())
        .or(all.first())
        .expect("no address")
}

/// Keys for nodes 1 and 2 and the `peers.toml` that pins them, in `dir`.
fn write_keys(dir: &Path) {
    let mut peers = String::from("[peers]\n");
    for n in 1..=2 {
        let (private, public) = generate_key().unwrap();
        write_key(&dir.join(format!("node-{n}.key")), &private);
        peers.push_str(&format!("{n} = [\"{public}\"]\n"));
    }
    std::fs::write(dir.join("peers.toml"), peers).unwrap();
}

fn write_key(path: &Path, private: &str) {
    std::fs::write(path, private).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn free_udp() -> SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn node(n: u32, address: SocketAddr, dir: &Path) -> (Runtime, QuicTransport) {
    let mut native = NativeConfig::default();
    native.runner = Some(Path::new(TARGET).join("release/bulkhead-runner"));
    // `NATIVE_SPIN_US`: the node's `[native] spin_us` (T1); unset, 20.
    if let Some(us) = std::env::var("NATIVE_SPIN_US")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        native.spin = Duration::from_micros(us);
    }
    let rt = Runtime::new(RuntimeConfig {
        node: n,
        max_instances: 64,
        address: address.to_string(),
        native,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let quic = QuicTransport::start(
        &rt,
        QuicConfig {
            bind: address,
            identity: NodeIdentity {
                key: dir.join(format!("node-{n}.key")),
                peers: dir.join("peers.toml"),
            },
            idle_timeout: Duration::from_secs(10),
            send_buffer_bytes: SEND_BUFFER_BYTES,
        },
    )
    .unwrap();
    (rt, quic)
}

/// Each instance's mailbox: `CLUSTER_MAILBOX`, or 65,536.
fn mailbox_max() -> usize {
    std::env::var("CLUSTER_MAILBOX").map_or(1 << 16, |v| v.parse().expect("CLUSTER_MAILBOX"))
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        memory_max: 64 << 20,
        mailbox_max: mailbox_max(),
        message_max_bytes: 1 << 20,
        // A native instance's casts are answered requests: its 25,000 to
        // another node outlast the default deadline on the Pi.
        callback_deadline: Duration::from_secs(600),
        ..InstanceSpec::new("bench", "0.1.0")
    }
}

fn text(body: &str) -> Envelope {
    Envelope::new("text/plain", body)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p) as usize]
}

async fn sequential(rt: &Runtime, to: Pid, label: &str) {
    let msg = text("echo:x");
    for _ in 0..1_000 {
        rt.call(to, msg.clone(), LONG).await.unwrap();
    }
    let mut us = Vec::with_capacity(SEQUENTIAL);
    let started = Instant::now();
    for _ in 0..SEQUENTIAL {
        let t = Instant::now();
        rt.call(to, msg.clone(), LONG).await.unwrap();
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let rate = SEQUENTIAL as f64 / started.elapsed().as_secs_f64();
    us.sort_by(f64::total_cmp);
    println!(
        "{label}: sequential calls {rate:.0}/s, p50 {:.1} us, p99 {:.1} us",
        percentile(&us, 0.5),
        percentile(&us, 0.99)
    );
}

/// `callers` tasks calling `msg` on `targets` in turn for [`TIMED`]; calls
/// per second.
async fn concurrent(rt: &Runtime, targets: &[Pid], callers: usize, msg: Envelope) -> f64 {
    let done = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + TIMED;
    let mut tasks = Vec::new();
    for c in 0..callers {
        let (rt, targets, done, msg) = (rt.clone(), targets.to_vec(), done.clone(), msg.clone());
        tasks.push(tokio::spawn(async move {
            let mut i = c;
            while Instant::now() < deadline {
                rt.call(targets[i % targets.len()], msg.clone(), LONG)
                    .await
                    .unwrap();
                done.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    done.load(Ordering::Relaxed) as f64 / TIMED.as_secs_f64()
}

/// The fixture's `count`: how many `count` messages the instance has had,
/// this one included. Retried while the casts before it still fill the
/// queue to the instance's node.
async fn count(rt: &Runtime, to: Pid) -> u64 {
    let reply = loop {
        match rt.call(to, text("count"), LONG).await {
            Err(CallError::Overloaded) => tokio::time::sleep(Duration::from_millis(1)).await,
            reply => break reply.unwrap(),
        }
    };
    String::from_utf8(reply.body).unwrap().parse().unwrap()
}

async fn casts(rt: &Runtime, targets: &[Pid], label: &str) {
    let mut before = 0u64;
    for to in targets {
        before += count(rt, *to).await;
    }
    let msg = text("count");
    let mut refused = 0u64;
    let started = Instant::now();
    for i in 0..CASTS {
        let to = targets[usize::try_from(i).unwrap() % targets.len()];
        if rt.cast(to, msg.clone()).is_err() {
            refused += 1;
            tokio::task::yield_now().await;
        }
        if i % 1_024 == 0 {
            tokio::task::yield_now().await;
        }
    }
    let mut after = 0u64;
    for to in targets {
        after += count(rt, *to).await;
    }
    // Each instance's second `count` call counted itself.
    let handled = after - before - targets.len() as u64;
    let secs = started.elapsed().as_secs_f64();
    println!(
        "{label}: casts {:.0} handled/s ({handled} of {CASTS} handled, {refused} refused when sent)",
        handled as f64 / secs
    );
}

/// `INSTANCES` instances of the fixture on `rt`, each answering.
async fn instances(rt: &Runtime, code: &Code) -> Vec<InstanceHandle> {
    let app = code.load(rt);
    let handles: Vec<InstanceHandle> = (0..INSTANCES)
        .map(|_| rt.spawn(&app, spec()).unwrap())
        .collect();
    for h in &handles {
        h.call(text("echo:ready"), LONG).await.unwrap();
    }
    handles
}

/// Wait until `rt` sees both nodes.
async fn joined(rt: &Runtime) {
    while rt.members().len() < 2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn local(workers: usize, code: &Code) {
    let dir = tempfile::tempdir().unwrap();
    write_keys(dir.path());
    let (a1, a2) = (free_udp(), free_udp());
    let (rt1, quic1) = node(1, a1, dir.path());
    let (rt2, quic2) = node(2, a2, dir.path());
    quic2.join_seed(a1).await.unwrap();
    joined(&rt1).await;
    joined(&rt2).await;
    let local = instances(&rt1, code).await;
    let targets = instances(&rt1, code).await;
    let remote = instances(&rt2, code).await;
    println!(
        "cluster_throughput: {workers} tokio workers for both nodes, QUIC over loopback, \
         {INSTANCES} instances per node, {}",
        code.kind()
    );
    let remote_pids: Vec<Pid> = remote.iter().map(InstanceHandle::pid).collect();
    bench(&rt1, &local, &targets, &remote_pids, code.native()).await;
    for h in local.iter().chain(&targets).chain(&remote) {
        h.kill();
    }
    quic1.shutdown();
    quic2.shutdown();
}

/// Node 2 at `bind` with its instances, their pids in `dir/pids`, until
/// killed.
async fn serve(dir: &Path, bind: SocketAddr, code: &Code) {
    let (rt, _quic) = node(2, bind, dir);
    let handles = instances(&rt, code).await;
    let pids: String = handles.iter().map(|h| format!("{}\n", h.pid())).collect();
    std::fs::write(dir.join("pids"), &pids).unwrap();
    println!(
        "cluster_throughput: node 2 at {bind}, {INSTANCES} instances, {}, pids in {}",
        code.kind(),
        dir.join("pids").display()
    );
    std::future::pending::<()>().await;
}

/// Node 1 at `bind`, joined to node 2 at `seed`: the runs, against the
/// pids in `dir/pids`.
async fn connect(workers: usize, dir: &Path, bind: SocketAddr, seed: SocketAddr, code: &Code) {
    let remote: Vec<Pid> = std::fs::read_to_string(dir.join("pids"))
        .unwrap()
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    let (rt, quic) = node(1, bind, dir);
    quic.join_seed(seed).await.unwrap();
    joined(&rt).await;
    let local = instances(&rt, code).await;
    let targets = instances(&rt, code).await;
    // Node 2 may still be dropping an earlier run's node 1.
    let ready = Instant::now();
    for to in &remote {
        while let Err(e) = rt.call(*to, text("echo:ready"), LONG).await {
            assert!(ready.elapsed() < LONG, "node 2 does not answer: {e}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    println!(
        "cluster_throughput: {workers} tokio workers on node 1, QUIC to node 2 at {seed} \
         (another process), {INSTANCES} instances per node, {}",
        code.kind()
    );
    bench(&rt, &local, &targets, &remote, code.native()).await;
    for h in local.iter().chain(&targets) {
        h.kill();
    }
    quic.shutdown();
}

/// The runs: node 1's host calling `local` on its own node and
/// `remote_pids` on node 2, then `local` calling `targets` on node 1 and
/// `remote_pids`. An instance busy calling cannot answer a call, so the
/// targets on node 1 are instances of their own.
async fn bench(
    rt1: &Runtime,
    local: &[InstanceHandle],
    targets: &[InstanceHandle],
    remote_pids: &[Pid],
    native: bool,
) {
    from_host(rt1, local, remote_pids).await;
    let neighbours: Vec<Pid> = targets.iter().map(InstanceHandle::pid).collect();
    app_sequential(&local[0], neighbours[0], "one node").await;
    app_sequential(&local[0], remote_pids[0], "two nodes").await;
    let one = app_concurrent(local, &neighbours).await;
    let two = app_concurrent(local, remote_pids).await;
    println!(
        "from instances, {} callers: one node {one:.0} calls/s, two nodes {two:.0} calls/s",
        local.len()
    );
    app_casts(rt1, local, &neighbours, "one node").await;
    app_casts(rt1, local, remote_pids, "two nodes").await;
    if native {
        for (to, label) in [(&neighbours[..], "one node"), (remote_pids, "two nodes")] {
            for kind in ["cast", "try", "wait"] {
                app_sends(rt1, local, to, kind, label).await;
            }
        }
    }
}

/// A pid as the fixture reads it.
fn fixture_pid(pid: Pid) -> String {
    format!("{}.{}.{}", pid.node, pid.id, pid.incarnation)
}

async fn ask(h: &InstanceHandle, body: &str) -> String {
    let reply = h.call(text(body), Duration::from_secs(600)).await.unwrap();
    let reply = String::from_utf8(reply.body).unwrap();
    assert!(!reply.starts_with("err:"), "{body}: {reply}");
    reply
}

/// `from` calling `to` one call at a time, timed by the app.
async fn app_sequential(from: &InstanceHandle, to: Pid, label: &str) {
    let to = fixture_pid(to);
    ask(from, &format!("calls:{to}:1000")).await;
    let n = SEQUENTIAL as u64;
    let reply = ask(from, &format!("calls:{to}:{n}")).await;
    let ns: Vec<f64> = reply.split(' ').map(|v| v.parse().unwrap()).collect();
    println!(
        "{label}, from an instance: sequential calls {:.0}/s, p50 {:.1} us, p99 {:.1} us",
        n as f64 / (ns[0] / 1e9),
        ns[1] / 1e3,
        ns[2] / 1e3
    );
}

/// Each of `from` calling its own target in `to`, all at once: calls per
/// second across them.
async fn app_concurrent(from: &[InstanceHandle], to: &[Pid]) -> f64 {
    let started = Instant::now();
    let tasks: Vec<_> = from
        .iter()
        .zip(to)
        .map(|(h, to)| {
            let (h, body) = (h.clone(), format!("calls:{}:{APP_CALLS}", fixture_pid(*to)));
            tokio::spawn(async move { ask(&h, &body).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    (from.len() as u64 * APP_CALLS) as f64 / started.elapsed().as_secs_f64()
}

/// Each of `from` casting `count` to its own target in `to`, all at once,
/// then the targets' counts read: casts handled per second.
async fn app_casts(rt: &Runtime, from: &[InstanceHandle], to: &[Pid], label: &str) {
    let mut before = 0u64;
    for t in to {
        before += count(rt, *t).await;
    }
    let started = Instant::now();
    let tasks: Vec<_> = from
        .iter()
        .zip(to)
        .map(|(h, to)| {
            let (h, body) = (h.clone(), format!("casts:{}:{APP_CASTS}", fixture_pid(*to)));
            tokio::spawn(async move { ask(&h, &body).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let mut after = 0u64;
    for t in to {
        after += count(rt, *t).await;
    }
    let handled = after - before - to.len() as u64;
    let sent = from.len() as u64 * APP_CASTS;
    println!(
        "{label}, from instances: casts {:.0} handled/s ({handled} of {sent} handled)",
        handled as f64 / started.elapsed().as_secs_f64()
    );
}

/// Each of `from` sending `count` to its own target in `to` by `kind`
/// (`cast`, `try`, `wait`), then the targets' counts read: casts handled
/// per second, sends that returned `Ok`, and whether each `Ok` of a
/// waited cast arrived.
async fn app_sends(rt: &Runtime, from: &[InstanceHandle], to: &[Pid], kind: &str, label: &str) {
    let mut before = 0u64;
    for t in to {
        before += count(rt, *t).await;
    }
    let started = Instant::now();
    let tasks: Vec<_> = from
        .iter()
        .zip(to)
        .map(|(h, to)| {
            let h = h.clone();
            let body = format!("sends:{}:{APP_CASTS}:{kind}", fixture_pid(*to));
            tokio::spawn(async move { ask(&h, &body).await })
        })
        .collect();
    let mut ok = 0u64;
    for t in tasks {
        let reply = t.await.unwrap();
        let (_, n) = reply
            .split_once(' ')
            .unwrap_or_else(|| panic!("sends: {reply}"));
        ok += n.parse::<u64>().unwrap();
    }
    let took = started.elapsed();
    let mut after = 0u64;
    for t in to {
        after += count(rt, *t).await;
    }
    let handled = after - before - to.len() as u64;
    let sent = from.len() as u64 * APP_CASTS;
    let check = match kind {
        "wait" if handled != ok => format!("; MISMATCH: {ok} Ok, {handled} arrived"),
        "wait" => "; every Ok arrived".to_string(),
        _ => String::new(),
    };
    println!(
        "{label}, from instances, {kind:>4}: {:.0} handled/s ({handled} of {sent} handled, {ok} Ok{check})",
        handled as f64 / took.as_secs_f64()
    );
}

async fn from_host(rt1: &Runtime, local: &[InstanceHandle], remote_pids: &[Pid]) {
    sequential(rt1, local[0].pid(), "one node").await;
    sequential(rt1, remote_pids[0], "two nodes").await;

    let local_pids: Vec<Pid> = local.iter().map(InstanceHandle::pid).collect();
    for callers in [16, 64] {
        let one = concurrent(rt1, &local_pids, callers, text("echo:x")).await;
        let two = concurrent(rt1, remote_pids, callers, text("echo:x")).await;
        println!("{callers} callers: one node {one:.0} calls/s, two nodes {two:.0} calls/s");
    }

    casts(rt1, &local_pids, "one node").await;
    casts(rt1, remote_pids, "two nodes").await;

    for size in [1usize << 10, 64 << 10] {
        // `mirror:` and padding: the reply is the same envelope back.
        let msg = Envelope::new("text/plain", {
            let mut b = b"mirror:".to_vec();
            b.resize(size, b'x');
            b
        });
        let one = concurrent(rt1, &local_pids, 16, msg.clone()).await;
        let two = concurrent(rt1, remote_pids, 16, msg).await;
        let mib = |calls: f64| calls * size as f64 / f64::from(1u32 << 20);
        println!(
            "{} KiB bodies, 16 callers: one node {one:.0} calls/s ({:.0} MiB/s each way), \
             two nodes {two:.0} calls/s ({:.0} MiB/s each way)",
            size >> 10,
            mib(one),
            mib(two)
        );
    }
}
