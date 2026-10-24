//! The replication soak: the failover soak's witness and pair, behind the
//! same proxies and under the same faults, with a `cluster` bucket on the
//! pair and the ledger example writing to it through both nodes.
//!
//! During each episode a writer puts a new key through one node or the
//! other at random intervals, and sorts each answer: acknowledged (`ok`),
//! refused (nothing was written: no primary could take it), or unknown (an
//! error that says the write may yet commit, or no answer at all, as from
//! a node stopped or killed mid-call). After each heal, once both copies
//! have settled, the run fails unless:
//!
//! - both nodes list the same keys (the copies agree),
//! - every acknowledged key is among them (no acknowledged write lost),
//! - every key among them was acknowledged or unknown (nothing appears
//!   that a client was told was refused).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::events::{EventLog, What};
use super::failover::{Args, Cluster, Setup, SetupApp};
use super::node::ADMIN_TIMEOUT;
use super::rng::Rng;

/// The app, from `examples/ledger/paddock.toml`.
const APP: &str = "ledger";

/// How long a write may take before its answer counts as unknown: past
/// the primary's own write timeout and a standby's forward timeout.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the copies have to agree after a heal.
const CONVERGE: Duration = Duration::from_secs(60);

/// Every write's outcome so far.
#[derive(Debug, Default)]
struct Ledger {
    acked: BTreeSet<String>,
    unknown: BTreeSet<String>,
    refused: BTreeSet<String>,
    next: u64,
}

/// How a write ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Acked,
    Refused,
    Unknown,
}

/// Sort a reply to `put`: an error that names no primary is a refusal,
/// any other error, or none at all, leaves the write unknown.
fn outcome(reply: Option<&str>) -> Outcome {
    match reply {
        Some("ok") => Outcome::Acked,
        Some(r) if r.contains("no primary with a lease") || r.contains("not running") => {
            Outcome::Refused
        }
        _ => Outcome::Unknown,
    }
}

/// What a check after a heal found wrong.
fn problems(ledger: &Ledger, keys: [&BTreeSet<String>; 2]) -> Vec<String> {
    let mut out = Vec::new();
    if keys[0] != keys[1] {
        let only1: Vec<_> = keys[0].difference(keys[1]).take(5).collect();
        let only2: Vec<_> = keys[1].difference(keys[0]).take(5).collect();
        out.push(format!(
            "the copies differ: only on node 1 {only1:?}, only on node 2 {only2:?}"
        ));
    }
    for (n, k) in (1..=2).zip(keys) {
        let lost: Vec<_> = ledger.acked.difference(k).take(5).collect();
        if !lost.is_empty() {
            out.push(format!("node {n} lacks acknowledged writes {lost:?}"));
        }
        let phantom: Vec<_> = k
            .iter()
            .filter(|key| !ledger.acked.contains(*key) && !ledger.unknown.contains(*key))
            .take(5)
            .collect();
        if !phantom.is_empty() {
            out.push(format!(
                "node {n} holds writes refused to the client {phantom:?}"
            ));
        }
    }
    out
}

/// Run, and return whether the run passed.
pub(crate) async fn run(args: &Args) -> anyhow::Result<bool> {
    let events = Arc::new(EventLog::create(&args.dir.join("events.jsonl"))?);
    let bucket = "[buckets.ledger]\ndurability = \"disk\"\nscope = \"cluster\"\n".to_string();
    let setup = Setup {
        apps: vec![SetupApp {
            name: APP,
            manifest: std::fs::read_to_string(args.root.join("examples/ledger/paddock.toml"))?,
            wasm: args
                .root
                .join("target/wasm32-wasip2/release/example_ledger.wasm"),
        }],
        pair_config: [bucket.clone(), bucket],
        failover_config: String::new(),
        policy: Some("[apps.ledger]\ngrant = [\"wasi:keyvalue\"]\nbuckets = [\"ledger\"]\n".into()),
    };
    let deadline = Instant::now() + Duration::from_secs_f64(args.minutes * 60.0);
    let mut rng = Rng::new(args.seed);
    let mut ledger = Ledger::default();
    let mut found = Vec::new();
    let mut episodes = 0;
    {
        let mut cluster = Cluster::start(args, &setup, events.clone()).await?;
        let sockets: Vec<PathBuf> = cluster.nodes[..2]
            .iter()
            .map(|n| n.socket().to_path_buf())
            .collect();
        tokio::time::sleep(args.timings.settle()).await;
        while Instant::now() < deadline && found.is_empty() {
            episodes += 1;
            let left = deadline.saturating_duration_since(Instant::now());
            let stop = Arc::new(AtomicBool::new(false));
            let writer = tokio::spawn(write(
                sockets.clone(),
                Rng::new(rng.next_u64()),
                ledger.next,
                stop.clone(),
            ));
            cluster.episode(&mut rng, args.episode.min(left)).await?;
            stop.store(true, Ordering::Relaxed);
            let written = writer.await?;
            ledger.next += written.len() as u64;
            for (key, o) in written {
                match o {
                    Outcome::Acked => ledger.acked.insert(key),
                    Outcome::Refused => ledger.refused.insert(key),
                    Outcome::Unknown => ledger.unknown.insert(key),
                };
            }
            cluster.heal().await?;
            tokio::time::sleep(args.timings.settle()).await;
            let keys = converge(&sockets).await;
            let wrong = problems(&ledger, [&keys[0], &keys[1]]);
            events.record(
                0,
                What::Note {
                    text: format!(
                        "after episode {episodes}: {} keys, {} acknowledged, {} unknown, {} refused; {}",
                        keys[0].len(),
                        ledger.acked.len(),
                        ledger.unknown.len(),
                        ledger.refused.len(),
                        if wrong.is_empty() { "ok".into() } else { wrong.join("; ") }
                    ),
                },
            );
            if !wrong.is_empty() {
                record_state(&cluster, &events).await;
            }
            found.extend(wrong);
        }
        // Dropping the cluster kills every node, recorded.
    }
    let mut text = format!(
        "{episodes} episodes, {} writes: {} acknowledged, {} unknown, {} refused\n",
        ledger.next,
        ledger.acked.len(),
        ledger.unknown.len(),
        ledger.refused.len()
    );
    if ledger.acked.is_empty() {
        found.push("no write was acknowledged".into());
    }
    for p in &found {
        let _ = writeln!(text, "PROBLEM: {p}");
    }
    std::fs::write(args.dir.join("result.txt"), &text)?;
    print!("{text}");
    Ok(found.is_empty())
}

/// Each node's members and the pair's failover state, into the events,
/// for a reader of a failed run.
async fn record_state(cluster: &Cluster, events: &EventLog) {
    for node in &cluster.nodes {
        let client = node.client();
        let members = tokio::time::timeout(ADMIN_TIMEOUT, client.members()).await;
        let failover = tokio::time::timeout(ADMIN_TIMEOUT, client.failover()).await;
        events.record(
            node.n,
            What::Note {
                text: format!("members {members:?}; failover {failover:?}"),
            },
        );
    }
}

/// Put new keys through a node chosen at random until `stop`; each key's
/// outcome.
async fn write(
    sockets: Vec<PathBuf>,
    mut rng: Rng,
    first: u64,
    stop: Arc<AtomicBool>,
) -> Vec<(String, Outcome)> {
    let mut out = Vec::new();
    let mut seq = first;
    while !stop.load(Ordering::Relaxed) {
        let socket = &sockets[usize::try_from(rng.range(0, 2)).unwrap_or(0)];
        let key = format!("w{seq:07}");
        seq += 1;
        let client = paddock::api::Client::new(socket);
        let call = client.call(
            APP,
            "text/plain",
            format!("put {key} {seq}").into_bytes(),
            u64::try_from(WRITE_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
        );
        let reply = match tokio::time::timeout(WRITE_TIMEOUT + ADMIN_TIMEOUT, call).await {
            Ok(Ok((_, body))) => Some(String::from_utf8_lossy(&body).into_owned()),
            _ => None,
        };
        out.push((key, outcome(reply.as_deref())));
        tokio::time::sleep(Duration::from_millis(rng.range(10, 200))).await;
    }
    out
}

/// Each pair node's keys, once both nodes list the same ones and their
/// logs stand at the same commit index, or as they are after
/// [`CONVERGE`].
async fn converge(sockets: &[PathBuf]) -> [BTreeSet<String>; 2] {
    let started = Instant::now();
    loop {
        let mut keys = [BTreeSet::new(), BTreeSet::new()];
        let mut commits = [None, None];
        for (i, socket) in sockets.iter().enumerate() {
            let client = paddock::api::Client::new(socket);
            if let Ok(Ok((_, body))) = tokio::time::timeout(
                ADMIN_TIMEOUT,
                client.call(APP, "text/plain", b"keys".to_vec(), 5_000),
            )
            .await
            {
                keys[i] = String::from_utf8_lossy(&body)
                    .split(',')
                    .filter(|k| k.starts_with('w'))
                    .map(str::to_string)
                    .collect();
            }
            if let Ok(Ok(f)) = tokio::time::timeout(ADMIN_TIMEOUT, client.failover()).await {
                commits[i] = f.replication.map(|r| (r.commit, r.last));
            }
        }
        let settled =
            matches!(commits, [Some((c1, l1)), Some((c2, l2))] if c1 == c2 && c1 == l1.max(l2));
        if (settled && keys[0] == keys[1]) || started.elapsed() > CONVERGE {
            return keys;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
#[path = "replication_tests.rs"]
mod replication_tests;
