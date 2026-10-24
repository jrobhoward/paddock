//! Restart and upgrade cost for `docs/BASELINE.md`: from a crash or a kill
//! to the restarted instance's first reply under a supervisor, and a hot
//! upgrade (`save-snapshot`, a new Store, `load-snapshot`) as its state
//! grows.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-chaos \
//!     --target-dir target/fixtures
//! cargo run --release -p paddock-host --example restart_cost -- [workers]
//! ```
//!
//! The chaos fixture upgrades to itself (0.1.0 to 0.2.0); its state is a
//! count plus `ballast:<kib>`, which its snapshot carries.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::{Duration, Instant};

use paddock_host::{
    Backoff, ChildSpec, ChildStart, Envelope, InstanceHandle, InstanceSpec, LoadedApp, Pid,
    RestartType, Runtime, RuntimeConfig, Strategy, SupervisorHandle, SupervisorSpec,
};

const RESTARTS: usize = 500;
const UPGRADES: usize = 50;
const BALLAST_KIB: [usize; 4] = [0, 64, 1024, 16 * 1024];
const LONG: Duration = Duration::from_secs(10);

fn main() {
    let workers: usize = std::env::args()
        .nth(1)
        .map_or(1, |a| a.parse().expect("workers"));
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

fn text(body: &str) -> Envelope {
    Envelope::new("text/plain", body)
}

async fn ask(h: &InstanceHandle, body: &str) -> String {
    String::from_utf8(h.call(text(body), LONG).await.unwrap().body).unwrap()
}

fn spec(version: &str) -> InstanceSpec {
    InstanceSpec {
        memory_max: 256 << 20,
        ..InstanceSpec::new("restart", version)
    }
}

async fn run(workers: usize, component: Vec<u8>) {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = rt.load(&component).unwrap();
    println!("tokio workers: {workers}; {RESTARTS} restarts each, {UPGRADES} upgrades per size");
    restarts(&rt, &app, "trap", "a crash (trap in handle-cast)").await;
    restarts(&rt, &app, "kill", "a kill").await;
    upgrades(&rt, &app).await;
}

/// The child's pid once it differs from `before`.
async fn next_pid(s: &SupervisorHandle, before: Pid) -> InstanceHandle {
    let mut rx = s.subscribe_children();
    loop {
        if let Some(h) = rx.borrow_and_update()[0].instance()
            && h.pid() != before
        {
            return h.clone();
        }
        rx.changed().await.unwrap();
    }
}

async fn restarts(rt: &Runtime, app: &LoadedApp, how: &str, label: &str) {
    let s = rt
        .start_supervisor(SupervisorSpec {
            name: "bench".into(),
            strategy: Strategy::OneForOne,
            intensity: u32::try_from(RESTARTS * 2).unwrap(),
            period: Duration::from_secs(1),
            backoff: Backoff::none(),
            children: vec![ChildSpec {
                id: "c".into(),
                restart: RestartType::Permanent,
                start: ChildStart::Instance {
                    app: app.clone(),
                    spec: spec("0.1.0"),
                },
            }],
        })
        .unwrap();
    let mut h = s.children()[0].instance().unwrap().clone();
    ask(&h, "echo:up").await;
    let mut samples = Vec::with_capacity(RESTARTS);
    for _ in 0..RESTARTS {
        let started = Instant::now();
        if how == "kill" {
            h.kill();
        } else {
            h.cast(text("trap")).unwrap();
        }
        let next = next_pid(&s, h.pid()).await;
        ask(&next, "echo:up").await;
        samples.push(started.elapsed());
        h = next;
    }
    show(
        &format!("{label} to the restarted instance's first reply"),
        samples,
    );
    s.shutdown();
    s.wait().await;
}

async fn upgrades(rt: &Runtime, app: &LoadedApp) {
    for kib in BALLAST_KIB {
        let h = rt.spawn(app, spec("0.1.0")).unwrap();
        ask(&h, &format!("ballast:{kib}")).await;
        let mut paused = Vec::with_capacity(UPGRADES);
        let mut whole = Vec::with_capacity(UPGRADES);
        let mut snapshot = Vec::with_capacity(UPGRADES);
        let mut versions = ["0.1.0", "0.2.0"];
        for _ in 0..UPGRADES {
            let t = Instant::now();
            h.snapshot().await.unwrap();
            snapshot.push(t.elapsed());

            let started = Instant::now();
            let pause = h
                .upgrade(app, spec(versions[1]), versions[0])
                .await
                .unwrap();
            ask(&h, "echo:up").await;
            whole.push(started.elapsed());
            paused.push(pause);
            versions.swap(0, 1);
        }
        let count: u64 = ask(&h, "count").await.parse().unwrap();
        assert_eq!(count, 1, "the count survived every upgrade");
        show(&format!("{kib:>6} KiB state: snapshot taken"), snapshot);
        show(
            &format!("{kib:>6} KiB state: upgrade, instance paused"),
            paused,
        );
        show(
            &format!("{kib:>6} KiB state: upgrade to first reply"),
            whole,
        );
        h.kill();
    }
}

fn show(label: &str, mut samples: Vec<Duration>) {
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
    println!(
        "{label}: p50 {:.1} µs, p99 {:.1} µs, max {:.1} µs",
        at(0.5),
        at(0.99),
        samples.last().unwrap().as_secs_f64() * 1e6
    );
}
