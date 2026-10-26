//! Spike S21: a TinyGo component against the Rust counter on the same
//! `Runtime`: compile time, start (spawn to first reply), and a call's
//! round trip. Both speak the counter's text protocol (`inc`).
//!
//! s21-bench <component.wasm>...

use std::time::{Duration, Instant};

use paddock_host::{Envelope, InstanceSpec, Runtime, RuntimeConfig};

const STARTS: usize = 200;
fn call_count() -> usize {
    std::env::var("S21_CALLS").map_or(20_000, |v| v.parse().unwrap())
}
const LONG: Duration = Duration::from_secs(10);

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        // S22: componentize-py components have 14.
        max_core_instances_per_component: std::env::var("S21_CORE_INSTANCES")
            .map_or(8, |v| v.parse().unwrap()),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let memory_max: u64 = std::env::var("S21_MEMORY_MAX_MIB")
        .map_or(64, |v| v.parse().unwrap())
        << 20;
    let spec = || InstanceSpec {
        memory_max,
        ..InstanceSpec::new("s21", "1.0.0")
    };
    for path in std::env::args().skip(1) {
        let component = std::fs::read(&path).unwrap();
        let t = Instant::now();
        let app = rt.load(&component).unwrap();
        let compile = t.elapsed();

        let mut starts = Vec::with_capacity(STARTS);
        for _ in 0..STARTS {
            let t = Instant::now();
            let h = rt.spawn(&app, spec()).unwrap();
            h.call(Envelope::new("text/plain", "get"), LONG).await.unwrap();
            starts.push(t.elapsed());
            h.kill();
        }

        let h = rt.spawn(&app, spec()).unwrap();
        let mut calls = Vec::with_capacity(call_count());
        for _ in 0..call_count() {
            let t = Instant::now();
            h.call(Envelope::new("text/plain", "inc"), LONG).await.unwrap();
            calls.push(t.elapsed());
        }
        let last = h.call(Envelope::new("text/plain", "get"), LONG).await.unwrap();

        println!("{path}");
        println!("  size {} KiB, compile {compile:?}", component.len() / 1024);
        println!("  start (spawn to first reply): {}", pct(starts));
        println!("  call round trip: {}", pct(calls));
        println!("  after: {}", String::from_utf8_lossy(&last.body));
        println!("  memory after the calls: {} KiB", h.memory_bytes() / 1024);
        h.kill();
    }
}

fn pct(mut v: Vec<Duration>) -> String {
    v.sort();
    let at = |p: f64| v[((v.len() as f64 - 1.0) * p) as usize];
    let slow = v.iter().filter(|d| **d > Duration::from_millis(1)).count();
    format!("p50 {:?}, p99 {:?}, over 1 ms: {slow}", at(0.5), at(0.99))
}
