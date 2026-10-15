//! What a publish to a process group costs, for `docs/BASELINE.md` (ROADMAP
//! B7): with 1 to 1,000 members on one node, the time of one `publish` call
//! (the host casts to every member before it returns), and deliveries per
//! second until every member has handled a burst of publishes.
//!
//! The guest is `paddock-fixture-sdk`: `join:<group>`, and
//! `publish:<group>:<text>` with `paddock:groups`; members keep each cast.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-sdk
//! cargo run --release -p paddock-host --example publish_cost -- 4
//! ```
//!
//! Argument: tokio worker threads (default 4).

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::{Duration, Instant};

use paddock_bundle::manifest::{Capability, GroupsScope};
use paddock_host::{Envelope, InstanceHandle, InstanceSpec, LoadedApp, Runtime, RuntimeConfig};

const LONG: Duration = Duration::from_secs(60);
const PUBLISHES: usize = 200;

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(4, |w| w.parse().expect("worker threads"));
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
        .block_on(async move { tokio::spawn(run(component, workers)).await })
        .unwrap();
}

async fn ask(h: &InstanceHandle, body: &str) -> String {
    let reply = h
        .call(Envelope::new("text/plain", body.to_string()), LONG)
        .await
        .unwrap();
    String::from_utf8(reply.body).unwrap()
}

fn spawn(rt: &Runtime, app: &LoadedApp) -> InstanceHandle {
    let mut spec = InstanceSpec::new("members", "1.0.0");
    spec.callback_deadline = LONG;
    spec.mailbox_max = 4 * PUBLISHES;
    spec.granted.insert(Capability::Groups);
    spec.services.groups.scope = GroupsScope {
        join: vec!["g".into()],
        publish: vec!["g".into()],
    };
    rt.spawn(app, spec).unwrap()
}

async fn run(component: Vec<u8>, workers: usize) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 1_100,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    let publisher = spawn(&rt, &app);
    println!("{workers} worker threads; {PUBLISHES} publishes per size");
    let mut members: Vec<InstanceHandle> = Vec::new();
    for target in [1usize, 10, 100, 1_000] {
        while members.len() < target {
            let m = spawn(&rt, &app);
            ask(&m, "join:g").await;
            members.push(m);
        }
        // One publish call: the host's fan-out.
        let mut calls = Vec::with_capacity(PUBLISHES);
        let started = Instant::now();
        for _ in 0..PUBLISHES {
            let t = Instant::now();
            let took = ask(&publisher, "publish:g:x").await;
            calls.push(t.elapsed());
            assert_eq!(took, target.to_string());
        }
        // A call to each member queues behind its casts: once all answer,
        // every publish has been handled.
        for m in &members {
            ask(m, "config:none").await;
        }
        let all = started.elapsed();
        calls.sort();
        println!(
            "{target:>5} members: publish call {:.1} µs (median), {:.0} deliveries/s handled",
            calls[calls.len() / 2].as_secs_f64() * 1e6,
            (PUBLISHES * target) as f64 / all.as_secs_f64()
        );
    }
    for m in members.iter().chain([&publisher]) {
        m.kill();
    }
}
