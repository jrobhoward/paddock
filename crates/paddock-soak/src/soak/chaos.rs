//! The chaos soak: the chaos example on one node, sent commands drawn from
//! the seed that echo, trap, spin past the callback deadline, allocate up to
//! past the memory cap, and log. The node must stay up, its admin API must
//! answer, the app must answer an echo at the end, and the node's resident
//! memory must not grow by more than a set amount after a warm-up.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::events::{EventLog, What};
use super::memory::rss_kib;
use super::node::{ADMIN_TIMEOUT, Node, bundle, socket_path};
use super::rng::Rng;

const APP: &str = "chaos";

/// A chaos run.
#[derive(Debug)]
pub(crate) struct Args {
    pub dir: PathBuf,
    pub root: PathBuf,
    pub paddock: PathBuf,
    pub seed: u64,
    pub minutes: f64,
    /// The most the node's resident memory may grow after the warm-up.
    pub rss_growth_mib: u64,
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Echo,
    Trap,
    Spin,
    Alloc,
    Log,
}

/// Counts for the report.
#[derive(Debug, Default)]
struct Tally {
    calls: u64,
    errors: u64,
    /// Errors by message, for the report.
    kinds: std::collections::BTreeMap<String, u64>,
    starts: u64,
    problems: Vec<String>,
}

/// Run, and return whether the run passed.
pub(crate) async fn run(args: &Args) -> anyhow::Result<bool> {
    let dir = &args.dir;
    let events = Arc::new(EventLog::create(&dir.join("events.jsonl"))?);
    let source = std::fs::read_to_string(args.root.join("examples/chaos/paddock.toml"))?;
    // The example gives up after 3 restarts in 10 s, and the default backoff
    // grows to 5 s; the soak crashes it far more often than that on purpose,
    // and wants it back quickly to keep calling it.
    let manifest = source.replace(
        "intensity = 3\n",
        "intensity = 1000\nbackoff = { initial_ms = 10, max_ms = 200, jitter = true }\n",
    );
    anyhow::ensure!(
        manifest != source,
        "examples/chaos/paddock.toml has no `intensity = 3` to raise"
    );
    let wasm = args
        .root
        .join("target/wasm32-wasip2/release/example_chaos.wasm");
    let (app, signer) = bundle(&args.paddock, dir, APP, &manifest, &wasm)?;
    let mut node = Node::new(1, &args.paddock, dir, socket_path("c", 1), events.clone());
    std::fs::write(
        node.config_path(),
        format!(
            "data_dir = {:?}\nadmin_socket = {:?}\ntrusted_keys = [{signer:?}]\nmax_instances = 32\n",
            dir.join("data-1"),
            node.socket()
        ),
    )?;
    node.spawn().await?;
    tokio::time::timeout(ADMIN_TIMEOUT, node.client().deploy(app))
        .await
        .map_err(|_| anyhow::anyhow!("deploy timed out"))??;

    let mut rng = Rng::new(args.seed);
    let mut tally = Tally::default();
    let begin = Instant::now();
    let length = Duration::from_secs_f64(args.minutes * 60.0);
    let warm_up = length / 10;
    let (mut rss_after_warm_up, mut rss_last, mut sampled) = (None, None, begin);
    while begin.elapsed() < length && tally.problems.is_empty() {
        step(&mut node, &mut rng, &mut tally).await;
        if sampled.elapsed() >= Duration::from_secs(10) {
            sampled = Instant::now();
            rss_last = node.pid().and_then(rss_kib);
            if begin.elapsed() >= warm_up && rss_after_warm_up.is_none() {
                rss_after_warm_up = rss_last;
            }
            events.record(
                1,
                What::Note {
                    text: format!("rss {rss_last:?} KiB"),
                },
            );
        }
    }
    if tally.problems.is_empty() {
        finish(&mut node, &mut tally).await;
    }

    let mut text = format!(
        "{} calls, {} answered with an error, {} restarts of the app by the runner\n",
        tally.calls, tally.errors, tally.starts
    );
    for (kind, count) in &tally.kinds {
        let _ = writeln!(text, "  {count:>8}  {kind}");
    }
    match (rss_after_warm_up, rss_last) {
        (Some(first), Some(last)) => {
            let growth_mib = last.saturating_sub(first) / 1024;
            let _ = writeln!(
                text,
                "node resident memory: {first} KiB after warm-up, {last} KiB at the end"
            );
            if growth_mib > args.rss_growth_mib {
                tally.problems.push(format!(
                    "resident memory grew {growth_mib} MiB, more than {} MiB",
                    args.rss_growth_mib
                ));
            }
        }
        _ => text.push_str("node resident memory: not sampled (no /proc, or a short run)\n"),
    }
    for p in &tally.problems {
        let _ = writeln!(text, "PROBLEM: {p}");
    }
    std::fs::write(dir.join("result.txt"), &text)?;
    print!("{text}");
    Ok(tally.problems.is_empty())
}

/// One command, after checking the node and the app.
async fn step(node: &mut Node, rng: &mut Rng, tally: &mut Tally) {
    if !node.alive() {
        tally.problems.push("the node exited".into());
        return;
    }
    let client = node.client();
    match tokio::time::timeout(ADMIN_TIMEOUT, client.status(APP)).await {
        Err(_) => {
            tally
                .problems
                .push(format!("admin API did not answer within {ADMIN_TIMEOUT:?}"));
            return;
        }
        Ok(Ok(d)) if d.summary.status != "running" => {
            tally.starts += 1;
            let _ = tokio::time::timeout(ADMIN_TIMEOUT, client.start(APP)).await;
        }
        Ok(_) => {}
    }
    let op = *rng.pick(&[
        (50, Op::Echo),
        (15, Op::Trap),
        (10, Op::Spin),
        (20, Op::Alloc),
        (5, Op::Log),
    ]);
    let token = rng.next_u64().to_string();
    let body = match op {
        Op::Echo => format!("echo:{token}"),
        Op::Trap => "trap".into(),
        Op::Spin => "spin".into(),
        // The manifest caps memory at 32 MiB; some of these exceed it.
        Op::Alloc => format!("alloc:{}", rng.range(1, 48)),
        Op::Log => format!("log:{}", rng.range(1, 1000)),
    };
    tally.calls += 1;
    // The callback deadline is 500 ms; a call that is not answered well
    // after it means the node is stuck.
    let call = client.call(APP, "text/plain", body.clone().into_bytes(), 3000);
    match tokio::time::timeout(Duration::from_secs(15), call).await {
        Err(_) => tally
            .problems
            .push(format!("`{body}` was not answered within 15 s")),
        Ok(Err(e)) => {
            tally.errors += 1;
            if matches!(&e, paddock::api::ClientError::Api(a) if a.kind == "not_found") {
                // Between restarts: give the supervisor a moment.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // Pids and numbers vary; the first words name the kind.
            let kind: String = e
                .to_string()
                .split_whitespace()
                .take(6)
                .collect::<Vec<_>>()
                .join(" ");
            *tally
                .kinds
                .entry(format!(
                    "{}: {kind}",
                    body.split(':').next().unwrap_or_default()
                ))
                .or_default() += 1;
        }
        Ok(Ok((_, reply))) => {
            if matches!(op, Op::Echo) && reply != token.as_bytes() {
                tally.problems.push(format!(
                    "`{body}` answered {:?}",
                    String::from_utf8_lossy(&reply)
                ));
            }
        }
    }
}

/// The app must answer an echo at the end, restarted if it needs to be.
async fn finish(node: &mut Node, tally: &mut Tally) {
    let client = node.client();
    for _ in 0..20 {
        if let Ok(Ok(d)) = tokio::time::timeout(ADMIN_TIMEOUT, client.status(APP)).await
            && d.summary.status != "running"
        {
            let _ = tokio::time::timeout(ADMIN_TIMEOUT, client.start(APP)).await;
        }
        let call = client.call(APP, "text/plain", b"echo:end".to_vec(), 3000);
        if let Ok(Ok((_, reply))) = tokio::time::timeout(ADMIN_TIMEOUT, call).await
            && reply == b"end"
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tally
        .problems
        .push("the app did not answer `echo:end` at the end".into());
}
