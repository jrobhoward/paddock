//! What a write to a replicated (`cluster`) bucket costs, for
//! `docs/BASELINE.md`: an HA pair (nodes 1 and 2) and a
//! witness (node 3) as three runtimes in one process, joined over the QUIC
//! transport on loopback, each pair node with its keyvalue database on
//! disk in a temporary directory under `target/` (a real filesystem with
//! real fsyncs, not tmpfs).
//!
//! The guest is `paddock-fixture-sdk`: `set:<bucket>:<key>:<value>` writes
//! with `wasi:keyvalue`.
//!
//! The pair can instead run on two machines, the primary with the
//! witness on one and the standby on the other, each in its own process
//! with its database under `DIR`. They hand over through the replicated
//! bucket: the primary writes `bench-go` when its runs are done, the
//! standby then runs its writer, writes `bench-done` and shuts its
//! transport down, and the primary runs alone.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-sdk
//! cargo run --release -p paddock-host --example replication_cost -- 4
//! # Between two machines: keys for the three nodes, copied to both.
//! cargo run --release -p paddock-host --example replication_cost -- 4 keys DIR
//! # The standby first (it waits for the others), then the primary.
//! cargo run --release -p paddock-host --example replication_cost -- 4 standby DIR B:7402 A:7403
//! cargo run --release -p paddock-host --example replication_cost -- 4 primary DIR A:7401 A:7403
//! ```
//!
//! Arguments: tokio worker threads (default 4), shared by the nodes in the
//! process; then nothing for all three over loopback, `keys DIR` (write
//! `node-<n>.key` for nodes 1 to 3 and `peers.toml`), `primary DIR
//! NODE1:PORT WITNESS:PORT` (nodes 1 and 3 at those addresses) or `standby
//! DIR NODE2:PORT WITNESS:PORT` (node 2, joining through the witness).
//! Each starts with an empty database (`DIR/data-<n>` removed).
//!
//! - **Disk bucket:** a node-scoped `disk` bucket on the primary, for
//!   comparison: one synced redb commit per write.
//! - **Replicated, sequential:** one writer on the primary, then one on the
//!   standby (forwarded to the primary): latency.
//! - **Replicated, concurrent:** 16 and 64 writers on the primary: writes
//!   per second (writes arriving together share a log append).
//! - **Alone:** the standby's transport shut down; once the primary has
//!   recorded it out of date and commits alone, one writer again.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use paddock_bundle::manifest::{AccessMode, Capability, Durability};
use paddock_host::failover::{Arbiter, FailoverConfig, FailoverHandle, Standing, WitnessConfig};
use paddock_host::transport::quic::{
    NodeIdentity, QuicConfig, QuicTransport, SEND_BUFFER_BYTES, generate_key,
};
use paddock_host::{
    Envelope, InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig, SharedBucket,
};

const LONG: Duration = Duration::from_secs(30);
const SEQUENTIAL: usize = 2_000;
const TIMED: Duration = Duration::from_secs(5);

/// What this process runs.
enum Mode {
    /// The three nodes, over loopback.
    Local,
    Keys(PathBuf),
    /// Nodes 1 and 3 at these addresses.
    Primary(PathBuf, SocketAddr, SocketAddr),
    /// Node 2 at the first address, joining the witness at the second.
    Standby(PathBuf, SocketAddr, SocketAddr),
}

fn main() {
    let mut args = std::env::args().skip(1);
    let workers: usize = args.next().map_or(4, |a| a.parse().expect("workers"));
    let rest: Vec<String> = args.collect();
    let mode = match rest.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => Mode::Local,
        ["keys", dir] => Mode::Keys(dir.into()),
        ["primary", dir, node1, witness] => {
            Mode::Primary(dir.into(), resolve(node1), resolve(witness))
        }
        ["standby", dir, node2, witness] => {
            Mode::Standby(dir.into(), resolve(node2), resolve(witness))
        }
        _ => panic!(
            "usage: replication_cost [WORKERS] [keys DIR | primary DIR NODE1:PORT WITNESS:PORT \
             | standby DIR NODE2:PORT WITNESS:PORT]"
        ),
    };
    if let Mode::Keys(dir) = &mode {
        std::fs::create_dir_all(dir).unwrap();
        write_keys(dir);
        return;
    }
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/paddock_fixture_sdk.wasm"
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
                    Mode::Primary(dir, node1, witness) => {
                        primary(workers, &dir, node1, witness, component).await;
                    }
                    Mode::Standby(dir, node2, witness) => {
                        standby(&dir, node2, witness, component).await;
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

/// Keys for nodes 1 to 3 and the `peers.toml` that pins them, in `dir`.
fn write_keys(dir: &Path) {
    let mut peers = String::from("[peers]\n");
    for n in 1..=3 {
        let (private, public) = generate_key().unwrap();
        write_key(&dir.join(format!("node-{n}.key")), &private);
        peers.push_str(&format!("{n} = [\"{public}\"]\n"));
    }
    std::fs::write(dir.join("peers.toml"), peers).unwrap();
}

/// Remove node `n`'s database under `dir`, for a run that starts empty.
fn fresh(dir: &Path, n: u32) {
    let data = dir.join(format!("data-{n}"));
    if data.exists() {
        std::fs::remove_dir_all(&data).unwrap();
    }
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
    let mut config = RuntimeConfig {
        node: n,
        max_instances: 128,
        address: address.to_string(),
        heartbeat_interval: Duration::from_millis(200),
        node_timeout: Duration::from_millis(1000),
        ..RuntimeConfig::default()
    };
    if n <= 2 {
        config.data_dir = Some(dir.join(format!("data-{n}")));
        config.buckets = BTreeMap::from([
            (
                "points".to_string(),
                SharedBucket {
                    durability: Durability::Replicated,
                    quota: None,
                },
            ),
            (
                "local".to_string(),
                SharedBucket {
                    durability: Durability::Disk,
                    quota: None,
                },
            ),
        ]);
    }
    let rt = Runtime::new(config).unwrap();
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

fn writer(rt: &Runtime, app: &LoadedApp) -> InstanceHandle {
    let mut spec = InstanceSpec {
        callback_deadline: Duration::from_secs(30),
        granted: [Capability::KeyValue].into(),
        ..InstanceSpec::new("bench", "1.0.0")
    };
    spec.services.keyvalue.shared = BTreeMap::from([
        ("points".to_string(), AccessMode::Rw),
        ("local".to_string(), AccessMode::Rw),
    ]);
    rt.spawn(app, spec).unwrap()
}

fn text(body: &str) -> Envelope {
    Envelope::new("text/plain", body)
}

fn body(e: &Envelope) -> String {
    String::from_utf8_lossy(&e.body).into_owned()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p) as usize]
}

/// Sequential writes to `bucket` through `w`: latency.
async fn sequential(w: &InstanceHandle, bucket: &str, label: &str) {
    let mut us = Vec::with_capacity(SEQUENTIAL);
    let started = Instant::now();
    for i in 0..SEQUENTIAL {
        let t = Instant::now();
        let reply = w
            .call(text(&format!("set:{bucket}:{label}-{i}:value")), LONG)
            .await
            .unwrap();
        assert_eq!(body(&reply), "ok", "{label}");
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let rate = SEQUENTIAL as f64 / started.elapsed().as_secs_f64();
    us.sort_by(f64::total_cmp);
    println!(
        "{label}: {rate:.0} writes/s, p50 {:.0} us, p99 {:.0} us",
        percentile(&us, 0.5),
        percentile(&us, 0.99)
    );
}

/// `writers` writing to `bucket` in turn for [`TIMED`]: writes per second.
async fn concurrent(writers: &[InstanceHandle], bucket: &str, label: &str) {
    let done = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + TIMED;
    let mut tasks = Vec::new();
    for (n, w) in writers.iter().enumerate() {
        let (w, done) = (w.clone(), done.clone());
        let bucket = bucket.to_string();
        let label = label.to_string();
        tasks.push(tokio::spawn(async move {
            let mut i = 0;
            while Instant::now() < deadline {
                let reply = w
                    .call(text(&format!("set:{bucket}:{label}-{n}-{i}:v")), LONG)
                    .await
                    .unwrap();
                assert_eq!(body(&reply), "ok");
                done.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let rate = done.load(Ordering::Relaxed) as f64 / TIMED.as_secs_f64();
    println!("{label}: {rate:.0} writes/s");
}

/// Wait until every runtime sees three members.
async fn joined(rts: &[&Runtime]) {
    while rts.iter().any(|rt| rt.members().len() < 3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until node 1 is primary with its standby caught up.
async fn primary_ready(rt1: &Runtime, f1: &FailoverHandle) {
    loop {
        if f1.leadership().may_run
            && rt1.replication_status().is_some_and(|s| {
                s.commit > 0 && s.commit == s.last && s.partner_matched == Some(s.last) && !s.alone
            })
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The writers on the primary, and the first runs: the disk bucket and
/// one replicated writer.
async fn primary_sequential(on_primary: &[InstanceHandle]) {
    sequential(&on_primary[0], "local", "disk bucket, one writer").await;
    sequential(
        &on_primary[0],
        "points",
        "replicated, one writer on the primary",
    )
    .await;
}

async fn primary_concurrent(on_primary: &[InstanceHandle]) {
    for n in [16, 64] {
        concurrent(
            &on_primary[..n],
            "local",
            &format!("disk bucket, {n} writers"),
        )
        .await;
        concurrent(
            &on_primary[..n],
            "points",
            &format!("replicated, {n} writers on the primary"),
        )
        .await;
    }
}

/// Once the standby is gone and the primary has recorded it out of date
/// and commits alone, one writer and then 16.
async fn primary_alone(rt1: &Runtime, on_primary: &[InstanceHandle]) {
    loop {
        let w = on_primary[0]
            .call(text("set:points:probe:v"), LONG)
            .await
            .unwrap();
        if body(&w) == "ok" && rt1.replication_status().is_some_and(|s| s.alone) {
            break;
        }
    }
    sequential(&on_primary[0], "points", "replicated, alone, one writer").await;
    concurrent(&on_primary[..16], "points", "replicated, alone, 16 writers").await;
}

/// Read `key` in `points` through `w` until it is set.
async fn wait_for(w: &InstanceHandle, key: &str) {
    let started = Instant::now();
    loop {
        let got = w.call(text(&format!("get:points:{key}")), LONG).await;
        if got.is_ok_and(|g| body(&g) != "none") {
            return;
        }
        assert!(started.elapsed() < Duration::from_secs(600), "no {key}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn local(workers: usize, component: Vec<u8>) {
    let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");
    let dir = tempfile::tempdir_in(target).unwrap();
    write_keys(dir.path());
    let addresses = [free_udp(), free_udp(), free_udp()];
    let (rt3, _quic3) = node(3, addresses[2], dir.path());
    let (rt1, quic1) = node(1, addresses[0], dir.path());
    let (rt2, quic2) = node(2, addresses[1], dir.path());
    for q in [&quic1, &quic2] {
        q.join_seed(addresses[2]).await.unwrap();
    }
    joined(&[&rt1, &rt2, &rt3]).await;
    rt3.serve_witness(WitnessConfig::new([1, 2], 1)).unwrap();
    let f1 = rt1
        .start_failover(FailoverConfig::new(2, Arbiter::Witness(3)))
        .unwrap();
    let _f2 = rt2
        .start_failover(FailoverConfig::new(1, Arbiter::Witness(3)))
        .unwrap();
    primary_ready(&rt1, &f1).await;
    let app1 = rt1.load(&component).unwrap();
    let app2 = rt2.load(&component).unwrap();
    let on_primary: Vec<InstanceHandle> = (0..64).map(|_| writer(&rt1, &app1)).collect();
    let on_standby = writer(&rt2, &app2);
    println!(
        "replication_cost: {workers} tokio workers for the three nodes, QUIC over loopback, \
         redb on {}",
        dir.path().display()
    );

    primary_sequential(&on_primary).await;
    sequential(
        &on_standby,
        "points",
        "replicated, one writer on the standby",
    )
    .await;
    primary_concurrent(&on_primary).await;

    // The standby goes away: the primary records it out of date and
    // commits alone.
    quic2.shutdown();
    primary_alone(&rt1, &on_primary).await;
}

/// Nodes 1 and 3 on this machine, node 2 on another.
async fn primary(
    workers: usize,
    dir: &Path,
    node1: SocketAddr,
    witness: SocketAddr,
    component: Vec<u8>,
) {
    fresh(dir, 1);
    let (rt3, _quic3) = node(3, witness, dir);
    let (rt1, quic1) = node(1, node1, dir);
    quic1.join_seed(witness).await.unwrap();
    joined(&[&rt1, &rt3]).await;
    rt3.serve_witness(WitnessConfig::new([1, 2], 1)).unwrap();
    let f1 = rt1
        .start_failover(FailoverConfig::new(2, Arbiter::Witness(3)))
        .unwrap();
    primary_ready(&rt1, &f1).await;
    let app1 = rt1.load(&component).unwrap();
    let on_primary: Vec<InstanceHandle> = (0..64).map(|_| writer(&rt1, &app1)).collect();
    println!(
        "replication_cost: {workers} tokio workers for the primary and the witness, QUIC to the \
         standby on another machine, redb on {}",
        dir.display()
    );

    primary_sequential(&on_primary).await;
    primary_concurrent(&on_primary).await;
    let go = on_primary[0]
        .call(text("set:points:bench-go:1"), LONG)
        .await
        .unwrap();
    assert_eq!(body(&go), "ok");
    println!("the standby's writer runs on the other machine");
    wait_for(&on_primary[0], "bench-done").await;
    primary_alone(&rt1, &on_primary).await;
}

/// Node 2 on this machine: standby until the primary's runs are done,
/// then one writer of its own, then gone.
async fn standby(dir: &Path, node2: SocketAddr, witness: SocketAddr, component: Vec<u8>) {
    fresh(dir, 2);
    let (rt2, quic2) = node(2, node2, dir);
    // The witness may not be up yet.
    while quic2.join_seed(witness).await.is_err() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    joined(&[&rt2]).await;
    let _f2 = rt2
        .start_failover(FailoverConfig::new(1, Arbiter::Witness(3)))
        .unwrap();
    while rt2
        .replication_status()
        .is_none_or(|s| s.standing != Standing::Standby)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let app2 = rt2.load(&component).unwrap();
    let on_standby = writer(&rt2, &app2);
    println!("replication_cost: standby, waiting for the primary's runs");
    wait_for(&on_standby, "bench-go").await;
    sequential(
        &on_standby,
        "points",
        "replicated, one writer on the standby",
    )
    .await;
    let done = on_standby
        .call(text("set:points:bench-done:1"), LONG)
        .await
        .unwrap();
    assert_eq!(body(&done), "ok");
    quic2.shutdown();
}
