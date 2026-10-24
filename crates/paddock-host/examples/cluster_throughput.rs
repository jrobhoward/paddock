//! Throughput between nodes, for `docs/BASELINE.md`: two
//! runtimes in one process joined over the QUIC transport on loopback, with
//! mutual TLS and pinned keys as `paddock node` uses. The host on node 1
//! calls and casts instances of the chaos fixture on node 2, and the same
//! on one node for comparison.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock-host --example cluster_throughput -- 4
//! ```
//!
//! Argument: tokio worker threads (default 4), shared by both nodes.
//!
//! - **Sequential calls:** one caller, `echo:x`, one at a time: latency.
//! - **Concurrent calls:** 16 and 64 callers over 16 instances: calls per
//!   second.
//! - **Casts:** `count` casts to 16 instances, then a call to each to read
//!   its count: casts handled per second, and how many were refused.
//! - **Payloads:** `mirror` calls with 1 KiB and 64 KiB bodies, 16 callers:
//!   MiB per second each way.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use paddock_host::transport::quic::{
    NodeIdentity, QuicConfig, QuicTransport, SEND_BUFFER_BYTES, generate_key,
};
use paddock_host::{Envelope, InstanceHandle, InstanceSpec, Pid, Runtime, RuntimeConfig};

const LONG: Duration = Duration::from_secs(30);
const SEQUENTIAL: usize = 20_000;
const TIMED: Duration = Duration::from_secs(5);
const INSTANCES: usize = 16;
const CASTS: u64 = 400_000;

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |a| a.parse().expect("workers"));
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
        .block_on(async move { tokio::spawn(run(workers, component)).await })
        .unwrap();
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

async fn casts(rt: &Runtime, targets: &[InstanceHandle], label: &str) {
    for h in targets {
        rt.call(h.pid(), text("count"), LONG).await.unwrap();
    }
    let msg = text("count");
    let mut refused = 0u64;
    let started = Instant::now();
    for i in 0..CASTS {
        let to = targets[usize::try_from(i).unwrap() % targets.len()].pid();
        if rt.cast(to, msg.clone()).is_err() {
            refused += 1;
            tokio::task::yield_now().await;
        }
        if i % 1_024 == 0 {
            tokio::task::yield_now().await;
        }
    }
    let mut handled = 0u64;
    for h in targets {
        let reply = rt.call(h.pid(), text("count"), LONG).await.unwrap();
        let n: u64 = String::from_utf8(reply.body).unwrap().parse().unwrap();
        handled += n - 2;
    }
    let secs = started.elapsed().as_secs_f64();
    println!(
        "{label}: casts {:.0} handled/s ({handled} of {CASTS} handled, {refused} refused when sent)",
        handled as f64 / secs
    );
}

async fn run(workers: usize, component: Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let mut peers = String::from("[peers]\n");
    for n in 1..=2 {
        let (private, public) = generate_key().unwrap();
        write_key(&dir.path().join(format!("node-{n}.key")), &private);
        peers.push_str(&format!("{n} = [\"{public}\"]\n"));
    }
    std::fs::write(dir.path().join("peers.toml"), peers).unwrap();
    let (a1, a2) = (free_udp(), free_udp());
    let (rt1, quic1) = node(1, a1, dir.path());
    let (rt2, quic2) = node(2, a2, dir.path());
    quic2.join_seed(a1).await.unwrap();
    while rt1.members().len() < 2 || rt2.members().len() < 2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let app1 = rt1.load(&component).unwrap();
    let app2 = rt2.load(&component).unwrap();
    let local: Vec<InstanceHandle> = (0..INSTANCES)
        .map(|_| rt1.spawn(&app1, spec()).unwrap())
        .collect();
    let remote: Vec<InstanceHandle> = (0..INSTANCES)
        .map(|_| rt2.spawn(&app2, spec()).unwrap())
        .collect();
    for h in local.iter().chain(&remote) {
        h.call(text("echo:ready"), LONG).await.unwrap();
    }
    println!(
        "cluster_throughput: {workers} tokio workers for both nodes, QUIC over loopback, \
         {INSTANCES} instances per node"
    );

    sequential(&rt1, local[0].pid(), "one node").await;
    sequential(&rt1, remote[0].pid(), "two nodes").await;

    let local_pids: Vec<Pid> = local.iter().map(InstanceHandle::pid).collect();
    let remote_pids: Vec<Pid> = remote.iter().map(InstanceHandle::pid).collect();
    for callers in [16, 64] {
        let one = concurrent(&rt1, &local_pids, callers, text("echo:x")).await;
        let two = concurrent(&rt1, &remote_pids, callers, text("echo:x")).await;
        println!("{callers} callers: one node {one:.0} calls/s, two nodes {two:.0} calls/s");
    }

    casts(&rt1, &local, "one node").await;
    casts(&rt1, &remote, "two nodes").await;

    for size in [1usize << 10, 64 << 10] {
        // `mirror:` and padding: the reply is the same envelope back.
        let msg = Envelope::new("text/plain", {
            let mut b = b"mirror:".to_vec();
            b.resize(size, b'x');
            b
        });
        let one = concurrent(&rt1, &local_pids, 16, msg.clone()).await;
        let two = concurrent(&rt1, &remote_pids, 16, msg).await;
        let mib = |calls: f64| calls * size as f64 / f64::from(1u32 << 20);
        println!(
            "{} KiB bodies, 16 callers: one node {one:.0} calls/s ({:.0} MiB/s each way), \
             two nodes {two:.0} calls/s ({:.0} MiB/s each way)",
            size >> 10,
            mib(one),
            mib(two)
        );
    }

    for h in local.iter().chain(&remote) {
        h.kill();
    }
    quic1.shutdown();
    quic2.shutdown();
}
