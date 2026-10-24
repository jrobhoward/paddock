//! Cold start for `docs/BASELINE.md`: compiling a component against its
//! size, and loading it again from the compiled-code cache
//! (`Runtime::load_cached`), as a node does after a restart.
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 -p paddock-fixture-echo \
//!     -p paddock-fixture-chaos -p paddock-fixture-sdk -p example-scada-control
//! cargo build --target wasm32-wasip2 -p paddock-fixture-sdk
//! cargo run --release -p paddock-host --example compile_cost -- [component.wasm ...]
//! ```
//!
//! Without arguments: the echo, chaos and SDK fixtures, the scada `control`
//! example, and the SDK fixture's debug build as a large component
//! (unoptimized code). Each figure is the median of 5 loads on one runtime.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use paddock_host::{Runtime, RuntimeConfig};

const RUNS: usize = 5;

fn main() {
    let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/wasm32-wasip2");
    let mut paths: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if paths.is_empty() {
        paths = [
            "release/paddock_fixture_echo.wasm",
            "release/paddock_fixture_chaos.wasm",
            "release/paddock_fixture_sdk.wasm",
            "release/example_scada_control.wasm",
            "debug/paddock_fixture_sdk.wasm",
        ]
        .iter()
        .map(|p| PathBuf::from(target).join(p))
        .collect();
    }
    // Created by the first cache write, private (0700), as the cache
    // requires; a directory group or others can write is refused.
    let cache_dir = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/compile_cost"
    ));
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 8,
        ..RuntimeConfig::default()
    })
    .unwrap();
    println!("median of {RUNS} loads each, one thread");
    println!(
        "{:<44} {:>9} {:>11} {:>13} {:>11} {:>10}",
        "component", "size KiB", "compile ms", "cache write ms", "cached ms", "cache KiB"
    );
    for path in paths {
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let name = path
            .strip_prefix(target)
            .unwrap_or(&path)
            .display()
            .to_string();
        let compile = median((0..RUNS).map(|_| timed(|| rt.load(&bytes).map(drop))));
        let Some(compile) = compile else {
            println!("{name:<44} not an app component; skipped");
            continue;
        };
        let cache = cache_dir.join(format!("{}.cwasm", name.replace('/', "_")));
        let written = median((0..RUNS).map(|_| {
            let _ = std::fs::remove_file(&cache);
            timed(|| rt.load_cached(&bytes, &cache).map(drop))
        }))
        .unwrap();
        let cached =
            median((0..RUNS).map(|_| timed(|| rt.load_cached(&bytes, &cache).map(drop)))).unwrap();
        let cache_kib = std::fs::metadata(&cache).map_or(0, |m| m.len() >> 10);
        println!(
            "{name:<44} {:>9} {:>11.1} {:>13.1} {:>11.2} {:>10}",
            bytes.len() >> 10,
            ms(compile),
            ms(written),
            ms(cached),
            cache_kib
        );
    }
}

fn timed<E>(f: impl FnOnce() -> Result<(), E>) -> Option<Duration> {
    let started = Instant::now();
    f().ok()?;
    Some(started.elapsed())
}

fn median(samples: impl Iterator<Item = Option<Duration>>) -> Option<Duration> {
    let mut all: Vec<Duration> = samples.collect::<Option<_>>()?;
    all.sort();
    Some(all[all.len() / 2])
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}
