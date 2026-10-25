//! Throughput between nodes, for `docs/BASELINE.md`: two
//! runtimes joined over the QUIC transport, with mutual TLS and pinned keys
//! as `paddock node` uses. The host on node 1 calls and casts instances of
//! the chaos fixture on node 2, and the same on node 1 alone for
//! comparison. Both nodes run in one process over loopback, or on two
//! machines.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock-host --example cluster_throughput -- 4
//! # Between two machines: keys for both nodes, copied to both.
//! cargo run --release -p paddock-host --example cluster_throughput -- 4 keys DIR
//! # Node 2 on one machine, at its own address; it writes DIR/pids.
//! cargo run --release -p paddock-host --example cluster_throughput -- 4 serve DIR ADDR:7400
//! # Node 1 on the other, with node 2's DIR/pids copied into its DIR.
//! cargo run --release -p paddock-host --example cluster_throughput -- 4 connect DIR ADDR:7400 NODE2:7400
//! ```
//!
//! Arguments: tokio worker threads (default 4), shared by both nodes in one
//! process; then nothing, `keys DIR` (write `node-1.key`, `node-2.key` and
//! `peers.toml`), `serve DIR ADDR:PORT` (node 2 with its instances, until
//! killed), or `connect DIR ADDR:PORT SEED:PORT` (node 1, joining node 2 at
//! the seed, then the runs below). Each node binds and advertises its own
//! address, so it must be one the other machine reaches.
//!
//! - **Sequential calls:** one caller, `echo:x`, one at a time: latency.
//! - **Concurrent calls:** 16 and 64 callers over 16 instances: calls per
//!   second.
//! - **Casts:** `count` casts to 16 instances, then a call to each to read
//!   its count: casts handled per second, and how many were refused.
//! - **Payloads:** `mirror` calls with 1 KiB and 64 KiB bodies, 16 callers:
//!   MiB per second each way.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use paddock_host::transport::quic::{
    NodeIdentity, QuicConfig, QuicTransport, SEND_BUFFER_BYTES, generate_key,
};
use paddock_host::{
    CallError, Envelope, InstanceHandle, InstanceSpec, Pid, Runtime, RuntimeConfig,
};

const LONG: Duration = Duration::from_secs(30);
const SEQUENTIAL: usize = 20_000;
const TIMED: Duration = Duration::from_secs(5);
const INSTANCES: usize = 16;
const CASTS: u64 = 400_000;

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
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/fixtures/wasm32-wasip2/release/paddock_fixture_chaos.wasm"
    );
    let component = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            tokio::spawn(async move {
                match mode {
                    Mode::Local => local(workers, component).await,
                    Mode::Serve(dir, bind) => serve(&dir, bind, component).await,
                    Mode::Connect(dir, bind, seed) => {
                        connect(workers, &dir, bind, seed, component).await;
                    }
                    Mode::Keys(_) => unreachable!(),
                }
            })
            .await
        })
        .unwrap();
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
    let rt = Runtime::new(RuntimeConfig {
        node: n,
        max_instances: 64,
        address: address.to_string(),
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

fn spec() -> InstanceSpec {
    InstanceSpec {
        memory_max: 64 << 20,
        mailbox_max: 1 << 16,
        message_max_bytes: 1 << 20,
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
async fn instances(rt: &Runtime, component: &[u8]) -> Vec<InstanceHandle> {
    let app = rt.load(component).unwrap();
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

async fn local(workers: usize, component: Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    write_keys(dir.path());
    let (a1, a2) = (free_udp(), free_udp());
    let (rt1, quic1) = node(1, a1, dir.path());
    let (rt2, quic2) = node(2, a2, dir.path());
    quic2.join_seed(a1).await.unwrap();
    joined(&rt1).await;
    joined(&rt2).await;
    let local = instances(&rt1, &component).await;
    let remote = instances(&rt2, &component).await;
    println!(
        "cluster_throughput: {workers} tokio workers for both nodes, QUIC over loopback, \
         {INSTANCES} instances per node"
    );
    let remote_pids: Vec<Pid> = remote.iter().map(InstanceHandle::pid).collect();
    bench(&rt1, &local, &remote_pids).await;
    for h in local.iter().chain(&remote) {
        h.kill();
    }
    quic1.shutdown();
    quic2.shutdown();
}

/// Node 2 at `bind` with its instances, their pids in `dir/pids`, until
/// killed.
async fn serve(dir: &Path, bind: SocketAddr, component: Vec<u8>) {
    let (rt, _quic) = node(2, bind, dir);
    let handles = instances(&rt, &component).await;
    let pids: String = handles.iter().map(|h| format!("{}\n", h.pid())).collect();
    std::fs::write(dir.join("pids"), &pids).unwrap();
    println!(
        "cluster_throughput: node 2 at {bind}, {INSTANCES} instances, pids in {}",
        dir.join("pids").display()
    );
    std::future::pending::<()>().await;
}

/// Node 1 at `bind`, joined to node 2 at `seed`: the runs, against the
/// pids in `dir/pids`.
async fn connect(
    workers: usize,
    dir: &Path,
    bind: SocketAddr,
    seed: SocketAddr,
    component: Vec<u8>,
) {
    let remote: Vec<Pid> = std::fs::read_to_string(dir.join("pids"))
        .unwrap()
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    let (rt, quic) = node(1, bind, dir);
    quic.join_seed(seed).await.unwrap();
    joined(&rt).await;
    let local = instances(&rt, &component).await;
    // Node 2 may still be dropping an earlier run's node 1.
    let ready = Instant::now();
    for to in &remote {
        while let Err(e) = rt.call(*to, text("echo:ready"), LONG).await {
            assert!(ready.elapsed() < LONG, "node 2 does not answer: {e}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    println!(
        "cluster_throughput: {workers} tokio workers on node 1, QUIC to node 2 on another \
         machine, {INSTANCES} instances per node"
    );
    bench(&rt, &local, &remote).await;
    for h in &local {
        h.kill();
    }
    quic.shutdown();
}

/// The runs: node 1's host calling `local` on its own node and
/// `remote_pids` on node 2.
async fn bench(rt1: &Runtime, local: &[InstanceHandle], remote_pids: &[Pid]) {
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
