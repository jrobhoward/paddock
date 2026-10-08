//! Calls through a node's admin API against calls inside the node, for
//! `docs/BASELINE.md`.
//!
//! Starts a node in a temporary directory with its admin socket, deploys the
//! chaos fixture, and times `echo:x` calls three ways: `Node::call` in the
//! same process (no HTTP), `Client::call` over the Unix socket one at a
//! time, and the same with several clients at once. The client opens a
//! connection per request, as the `paddock` CLI and `curl` do.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock --example admin_latency -- [workers]
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use minisign::KeyPair;
use paddock::api::Client;
use paddock::node::{LogRing, Node, NodeConfig};
use paddock_host::Envelope;
use rustbridge_bundle::{BundleBuilder, Manifest, Platform};

const CALLS: usize = 20_000;
const CLIENTS: usize = 16;
const LONG: Duration = Duration::from_secs(10);

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |a| a.parse().expect("workers"));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap()
        // On a worker, as the server's tasks are: with one worker, a call and
        // its reply stay on the same thread.
        .block_on(async move { tokio::spawn(run(workers)).await })
        .unwrap();
}

fn bundle(dir: &std::path::Path, key: &KeyPair) -> Vec<u8> {
    let component = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/fixtures/wasm32-wasip2/release/paddock_fixture_chaos.wasm"
    );
    let out = dir.join("chaos.rbp");
    BundleBuilder::new(Manifest::new("chaos", "1.0.0"))
        .add_component(
            Platform::Wasm32Wasip2,
            std::path::Path::new(component),
            "paddock:runtime/app@0.1.0",
        )
        .unwrap_or_else(|e| panic!("{component}: {e}"))
        .with_extension("appserver", serde_json::json!({}))
        .with_signing_key(key.pk.to_base64(), key.sk.clone())
        .write(&out)
        .unwrap();
    std::fs::read(out).unwrap()
}

fn report(label: &str, mut samples: Vec<Duration>, elapsed: Duration) {
    samples.sort();
    let at = |q: f64| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let i = ((samples.len() as f64 - 1.0) * q).round() as usize;
        samples[i].as_secs_f64() * 1e6
    };
    #[allow(clippy::cast_precision_loss)]
    let rate = samples.len() as f64 / elapsed.as_secs_f64();
    println!(
        "{label}: p50 {:.1} µs, p99 {:.1} µs, p99.9 {:.1} µs; {rate:.0} calls/s",
        at(0.5),
        at(0.99),
        at(0.999)
    );
}

async fn run(workers: usize) {
    let dir = tempfile::TempDir::new().unwrap();
    let key = KeyPair::generate_unencrypted_keypair().unwrap();
    let config = format!(
        "data_dir = {:?}\nmax_instances = 8\ntrusted_keys = [{:?}]\n",
        dir.path().join("data"),
        key.pk.to_base64()
    );
    let node = Arc::new(
        Node::open(
            NodeConfig::from_toml_str(&config).unwrap(),
            LogRing::new(100),
        )
        .await
        .unwrap(),
    );
    let socket = node.socket();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn({
        let node = node.clone();
        async move {
            paddock::node::serve(node, async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
        }
    });
    while !socket.exists() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    node.deploy(&bundle(dir.path(), &key)).await.unwrap();
    let client = Client::new(&socket);
    println!("tokio workers: {workers}; {CALLS} calls each");

    // In process: the node's own routing, no HTTP.
    let mut samples = Vec::with_capacity(CALLS);
    let started = Instant::now();
    for _ in 0..CALLS {
        let t = Instant::now();
        node.call("chaos", Envelope::new("text/plain", "echo:x"), LONG)
            .await
            .unwrap();
        samples.push(t.elapsed());
    }
    report("in process (Node::call)", samples, started.elapsed());

    // One client over the socket.
    let mut samples = Vec::with_capacity(CALLS);
    let started = Instant::now();
    for _ in 0..CALLS {
        let t = Instant::now();
        client
            .call("chaos", "text/plain", b"echo:x".to_vec(), 10_000)
            .await
            .unwrap();
        samples.push(t.elapsed());
    }
    report("admin API, 1 client", samples, started.elapsed());

    // Several clients at once.
    let started = Instant::now();
    let tasks: Vec<_> = (0..CLIENTS)
        .map(|_| {
            let client = Client::new(&socket);
            tokio::spawn(async move {
                let mut samples = Vec::with_capacity(CALLS / CLIENTS);
                for _ in 0..CALLS / CLIENTS {
                    let t = Instant::now();
                    client
                        .call("chaos", "text/plain", b"echo:x".to_vec(), 10_000)
                        .await
                        .unwrap();
                    samples.push(t.elapsed());
                }
                samples
            })
        })
        .collect();
    let mut samples = Vec::with_capacity(CALLS);
    for t in tasks {
        samples.extend(t.await.unwrap());
    }
    report(
        &format!("admin API, {CLIENTS} clients"),
        samples,
        started.elapsed(),
    );

    node.shutdown().await;
    let _ = stop.send(());
    server.await.unwrap();
}
