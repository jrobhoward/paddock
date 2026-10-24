//! Replicated buckets on an HA pair (nodes 1 and 2) with a witness (node
//! 3), against spike S20's properties: a write acknowledged
//! to a client is never lost over a failure, the two copies agree once
//! healed, and a linearizable read sees every acknowledged write.
//!
//! Each pair node keeps its buckets on disk in its own temporary
//! directory, so a node can be stopped and started again from it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use paddock_bundle::manifest::Durability;
use paddock_host::failover::{Arbiter, FailoverConfig, FailoverHandle, Standing, WitnessConfig};
use paddock_host::transport::wire;
use paddock_host::{
    FIRST_FORMATS, FinalizeRefused, Finalized, InstanceHandle, Role, RuntimeConfig, SharedBucket,
};

use crate::common::{ask, body, text};
use crate::harness::{Sim, SimNode, pass, simulate_with};

const LEASE: Duration = Duration::from_millis(1000);
const MARGIN: Duration = Duration::from_millis(200);
const SERIAL: &str = "serial";
const BUCKET: &str = "points";

/// The pair nodes' configuration: a data directory each, and the
/// replicated shared bucket.
fn configure(dirs: &[PathBuf; 2]) -> impl Fn(u32, &mut RuntimeConfig) + '_ {
    move |node, config| {
        if node <= 2 {
            replicated(&dirs[usize::try_from(node - 1).unwrap()], config);
        }
    }
}

fn replicated(dir: &Path, config: &mut RuntimeConfig) {
    config.data_dir = Some(dir.to_path_buf());
    config.buckets = BTreeMap::from([(
        BUCKET.to_string(),
        SharedBucket {
            durability: Durability::Replicated,
            quota: None,
        },
    )]);
}

fn dirs() -> (tempfile::TempDir, [PathBuf; 2]) {
    let root = tempfile::tempdir().unwrap();
    let dirs = [root.path().join("1"), root.path().join("2")];
    (root, dirs)
}

fn failover(node: u32) -> FailoverConfig {
    let mut cfg = FailoverConfig::new(3 - node, Arbiter::Witness(3));
    cfg.lease = LEASE;
    cfg.margin = MARGIN;
    cfg
}

/// Serve the witness on node 3 and start failover on 1 and 2, with a
/// serial line between them.
fn start(sim: &Sim) -> Vec<FailoverHandle> {
    sim.mesh.add_path(SERIAL);
    sim.mesh.cut_path(1, 3, SERIAL);
    sim.mesh.cut_path(2, 3, SERIAL);
    let mut witness = WitnessConfig::new([1, 2], 1);
    witness.lease = LEASE;
    witness.margin = MARGIN;
    sim.node(3).rt.serve_witness(witness).unwrap();
    (1..=2)
        .map(|node| sim.node(node).rt.start_failover(failover(node)).unwrap())
        .collect()
}

/// Wait until some node runs as primary with its lease and its log has
/// committed this epoch's no-op; that node.
async fn primary(sim: &Sim) -> u32 {
    for _ in 0..200 {
        for node in 1..=2 {
            if let Some(s) = sim.node(node).rt.replication_status()
                && s.standing == Standing::Primary
                && s.commit > 0
                && s.commit == s.last
            {
                return node;
            }
        }
        pass(50).await;
    }
    panic!("no primary");
}

/// Cut `node` off from everything: links dropped, serial line cut.
fn isolate(sim: &Sim, node: u32) {
    for other in (1..=3).filter(|&o| o != node) {
        sim.mesh.partition(node, other);
        sim.mesh.cut_path(node, other, SERIAL);
    }
}

fn heal(sim: &Sim) {
    sim.mesh.heal_all();
}

/// Wait until both copies have committed the same last entry.
async fn settled(sim: &Sim) {
    for _ in 0..400 {
        let a = sim.node(1).rt.replication_status();
        let b = sim.node(2).rt.replication_status();
        if let (Some(a), Some(b)) = (a, b)
            && a.commit == b.commit
            && a.commit == a.last.max(b.last)
        {
            return;
        }
        pass(50).await;
    }
    panic!(
        "copies did not settle: {:?} {:?}",
        sim.node(1).rt.replication_status(),
        sim.node(2).rt.replication_status()
    );
}

async fn set(kv: &InstanceHandle, key: &str, value: &str) -> String {
    ask(kv, &format!("set:{BUCKET}:{key}:{value}")).await
}

async fn get(kv: &InstanceHandle, key: &str) -> String {
    ask(kv, &format!("get:{BUCKET}:{key}")).await
}

async fn keys(kv: &InstanceHandle) -> String {
    ask(kv, &format!("keys:{BUCKET}")).await
}

#[test]
fn replicated____writes_on_either_node____both_copies_agree() {
    let (_root, dirs) = dirs();
    let (on_primary, on_standby, latest, keys_1, keys_2) =
        simulate_with(61, 3, configure(&dirs), |sim| async move {
            let _handles = start(&sim);
            let p = primary(&sim).await;
            let s = 3 - p;
            let kv_p = sim.node(p).spawn_kv(BUCKET);
            let kv_s = sim.node(s).spawn_kv(BUCKET);

            let on_primary = set(&kv_p, "a", "1").await;
            let on_standby = set(&kv_s, "b", "2").await;
            let latest = ask(&kv_s, &format!("latest:{BUCKET}:b")).await;
            settled(&sim).await;
            let k1 = keys(&sim.node(1).spawn_kv(BUCKET)).await;
            let k2 = keys(&sim.node(2).spawn_kv(BUCKET)).await;
            (on_primary, on_standby, latest, k1, k2)
        });

    assert_eq!((on_primary.as_str(), on_standby.as_str()), ("ok", "ok"));
    assert_eq!(latest, "2@2");
    assert_eq!(keys_1, "a,b");
    assert_eq!(keys_2, "a,b");
}

#[test]
fn replicated____primary_cut_off____standby_promoted_with_every_acknowledged_write() {
    let (_root, dirs) = dirs();
    let (acked, after_failover, alone_write, recorded, healed) =
        simulate_with(62, 3, configure(&dirs), |sim| async move {
            let handles = start(&sim);
            let p = primary(&sim).await;
            let s = 3 - p;
            let kv_p = sim.node(p).spawn_kv(BUCKET);
            let mut acked = Vec::new();
            for i in 0..5 {
                acked.push(set(&kv_p, &format!("k{i}"), "v").await);
            }

            isolate(&sim, p);
            pass(4_000).await;
            let kv_s = sim.node(s).spawn_kv(BUCKET);
            let after_failover = keys(&kv_s).await;
            let alone_write = set(&kv_s, "k5", "v").await;
            let recorded = handles[usize::try_from(s - 1).unwrap()]
                .leadership()
                .partner_outdated;
            heal(&sim);
            settled(&sim).await;
            pass(1_000).await;
            let healed = keys(&sim.node(p).spawn_kv(BUCKET)).await;
            (acked, after_failover, alone_write, recorded, healed)
        });

    assert!(acked.iter().all(|r| r == "ok"), "{acked:?}");
    assert_eq!(after_failover, "k0,k1,k2,k3,k4");
    assert_eq!(alone_write, "ok");
    assert!(
        recorded,
        "the new primary committed alone only once recorded"
    );
    assert_eq!(healed, "k0,k1,k2,k3,k4,k5");
}

#[test]
fn replicated____partner_recorded_out_of_date____not_promoted_until_caught_up() {
    let (_root, dirs) = dirs();
    let (standing_while_outdated, write_while_outdated, standing_after, keys_after) =
        simulate_with(63, 3, configure(&dirs), |sim| async move {
            let handles = start(&sim);
            let p = primary(&sim).await;
            let s = 3 - p;
            // The standby is cut off; the primary commits alone.
            isolate(&sim, s);
            pass(500).await;
            let kv_p = sim.node(p).spawn_kv(BUCKET);
            assert_eq!(set(&kv_p, "only-on-primary", "v").await, "ok");
            // Then the primary is cut off too, and the standby reaches the
            // witness again, but not the primary.
            isolate(&sim, p);
            sim.mesh.heal(s, 3);
            pass(5_000).await;
            let standing_while_outdated = handles[usize::try_from(s - 1).unwrap()]
                .leadership()
                .standing;
            let kv_s = sim.node(s).spawn_kv(BUCKET);
            let write_while_outdated = set(&kv_s, "x", "y").await;
            // Everything heals: the old primary leads again, the standby
            // catches up, and the record is cleared.
            heal(&sim);
            settled(&sim).await;
            pass(1_000).await;
            let standing_after = handles[usize::try_from(s - 1).unwrap()]
                .leadership()
                .standing;
            (
                standing_while_outdated,
                write_while_outdated,
                standing_after,
                keys(&kv_s).await,
            )
        });

    assert_ne!(standing_while_outdated, Standing::Primary);
    assert!(
        write_while_outdated.starts_with("err:"),
        "{write_while_outdated}"
    );
    assert_eq!(standing_after, Standing::Standby);
    assert_eq!(keys_after, "only-on-primary");
}

#[test]
fn replicated____primary_stopped_and_started_again____rejoins_from_its_disk() {
    let (_root, dirs) = dirs();
    let first = dirs[0].clone();
    let (keys_on_restarted, value) = simulate_with(64, 3, configure(&dirs), |mut sim| async move {
        let handles = start(&sim);
        assert_eq!(primary(&sim).await, 1);
        let kv_1 = sim.node(1).spawn_kv(BUCKET);
        assert_eq!(set(&kv_1, "before", "1").await, "ok");
        // Instances hold their runtime; none may outlive the stop.
        kv_1.kill();
        drop(kv_1);

        // Node 1 stops; node 2 takes over and writes.
        drop(handles);
        isolate(&sim, 1);
        let stopped = sim.nodes.remove(0);
        drop(stopped);
        pass(4_000).await;
        let kv_2 = sim.node(1).spawn_kv(BUCKET);
        assert_eq!(set(&kv_2, "during", "2").await, "ok");
        // Node 1 starts again from its directory (its index in
        // `sim.nodes` is now 2's, so it goes back first).
        let restarted = SimNode::start_with(1, 2, Role::Member, |c| replicated(&first, c));
        sim.mesh.heal_all();
        sim.mesh.join(&restarted.rt).unwrap();
        sim.nodes.insert(0, restarted);
        let _h = sim.node(1).rt.start_failover(failover(1)).unwrap();
        settled(&sim).await;
        let kv_1 = sim.node(1).spawn_kv(BUCKET);
        (keys(&kv_1).await, get(&kv_1, "during").await)
    });

    assert_eq!(keys_on_restarted, "before,during");
    assert_eq!(value, "2");
}

/// Seeded random faults, one at a time, with writes from both nodes:
/// every acknowledged write survives, and the copies agree once healed.
#[test]
fn replicated____random_single_faults____no_acknowledged_write_lost() {
    for seed in 70..74 {
        let (_root, dirs) = dirs();
        let (acked, missing, agree) = simulate_with(seed, 3, configure(&dirs), |sim| async move {
            let _handles = start(&sim);
            primary(&sim).await;
            let kvs = [sim.node(1).spawn_kv(BUCKET), sim.node(2).spawn_kv(BUCKET)];
            let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut next = || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            };
            let mut acked = Vec::new();
            let mut fault = None;
            for i in 0..40 {
                match next() % 6 {
                    0 if fault.is_none() => {
                        let f = next() % 4;
                        match f {
                            0 => sim.mesh.partition(1, 2),
                            1 => sim.mesh.cut_path(1, 2, SERIAL),
                            2 => sim.mesh.partition(1, 3),
                            _ => sim.mesh.partition(2, 3),
                        }
                        fault = Some(f);
                    }
                    1 if fault.is_some() => {
                        heal(&sim);
                        fault = None;
                    }
                    _ => {
                        let node = usize::try_from(next() % 2).unwrap();
                        let key = format!("w{i:02}");
                        if set(&kvs[node], &key, &format!("{node}")).await == "ok" {
                            acked.push(key);
                        }
                    }
                }
                pass(next() % 1_500).await;
            }
            heal(&sim);
            settled(&sim).await;
            let present = keys(&kvs[0]).await;
            let missing: Vec<String> = acked
                .iter()
                .filter(|k| !present.split(',').any(|p| p == k.as_str()))
                .cloned()
                .collect();
            let agree = present == keys(&kvs[1]).await;
            (acked, missing, agree)
        });

        assert!(!acked.is_empty(), "seed {seed}: no write acknowledged");
        assert!(missing.is_empty(), "seed {seed}: lost {missing:?}");
        assert!(agree, "seed {seed}: copies differ");
    }
}

#[test]
fn replicated____no_ack_and_no_witness____write_answered_within_the_timeout() {
    let (_root, dirs) = dirs();
    let (reply, waited, after_heal) = simulate_with(65, 3, configure(&dirs), |sim| async move {
        let _handles = start(&sim);
        let p = primary(&sim).await;
        let s = 3 - p;
        let kv = sim.node(p).spawn_kv(BUCKET);
        // The serial line still renews the lease, but nothing carries the
        // entry to the standby, and the witness cannot record it.
        sim.mesh.partition(p, s);
        sim.mesh.partition(p, 3);
        pass(200).await;
        let started = tokio::time::Instant::now();
        let call = kv.call(
            text(&format!("set:{BUCKET}:stuck:v")),
            Duration::from_secs(30),
        );
        let reply = body(&call.await.unwrap());
        let waited = started.elapsed();
        heal(&sim);
        settled(&sim).await;
        (reply, waited, keys(&kv).await)
    });

    assert!(
        reply.starts_with("err:") && reply.contains("may yet commit"),
        "{reply}"
    );
    assert!(waited >= Duration::from_secs(5), "{waited:?}");
    assert!(waited < Duration::from_secs(7), "{waited:?}");
    assert_eq!(after_heal, "stuck", "its entry committed once healed");
}

/// The pair nodes' configuration for apps' own replicated buckets: a data
/// directory each, no shared buckets.
fn configure_apps(dirs: &[PathBuf; 2]) -> impl Fn(u32, &mut RuntimeConfig) + '_ {
    move |node, config| {
        if node <= 2 {
            config.data_dir = Some(dirs[usize::try_from(node - 1).unwrap()].clone());
        }
    }
}

/// [`start`] with `replication` set, for apps' replicated buckets.
fn start_replicating(sim: &Sim) -> Vec<FailoverHandle> {
    sim.mesh.add_path(SERIAL);
    sim.mesh.cut_path(1, 3, SERIAL);
    sim.mesh.cut_path(2, 3, SERIAL);
    let mut witness = WitnessConfig::new([1, 2], 1);
    witness.lease = LEASE;
    witness.margin = MARGIN;
    sim.node(3).rt.serve_witness(witness).unwrap();
    (1..=2)
        .map(|node| {
            let mut cfg = failover(node);
            cfg.replication = true;
            sim.node(node).rt.start_failover(cfg).unwrap()
        })
        .collect()
}

#[test]
fn replicated____app_buckets_primary_cut_off____each_app_keeps_its_writes() {
    let (_root, dirs) = dirs();
    let (acked, a_after, b_after, ready, healed) =
        simulate_with(63, 3, configure_apps(&dirs), |sim| async move {
            let _handles = start_replicating(&sim);
            let p = primary(&sim).await;
            let s = 3 - p;
            let a = sim.node(p).spawn_kv_replicated("a", BUCKET);
            let b = sim.node(s).spawn_kv_replicated("b", BUCKET);
            let acked = vec![set(&a, "k", "from-a").await, set(&b, "k", "from-b").await];

            isolate(&sim, p);
            pass(4_000).await;
            let a_after = get(&sim.node(s).spawn_kv_replicated("a", BUCKET), "k").await;
            let b_after = get(&sim.node(s).spawn_kv_replicated("b", BUCKET), "k").await;
            let ready = sim.node(s).rt.replication_status().is_some_and(|r| r.ready);
            heal(&sim);
            settled(&sim).await;
            let healed = keys(&sim.node(p).spawn_kv_replicated("a", BUCKET)).await;
            (acked, a_after, b_after, ready, healed)
        });

    assert_eq!(acked, ["ok", "ok"]);
    assert_eq!(a_after, "from-a");
    assert_eq!(b_after, "from-b");
    assert!(ready, "the new primary's copy is current");
    assert_eq!(healed, "k");
}

#[test]
fn replicated____pair_without_replication____runs_none() {
    let (_root, dirs) = dirs();
    let statuses = simulate_with(64, 3, configure_apps(&dirs), |sim| async move {
        let _handles = start(&sim);
        pass(3_000).await;
        [
            sim.node(1).rt.replication_status(),
            sim.node(2).rt.replication_status(),
        ]
    });

    assert_eq!(statuses, [None, None]);
}

/// The pair's version for what is kept: refused while the
/// witness is one release older, then raised on both copies once every
/// node speaks this build's version, from either node; a second raise
/// changes nothing.
#[test]
fn finalize_formats____every_node_at_this_version____raised_on_both_copies() {
    let (_root, dirs) = dirs();
    let (before, refused, raised, again, kept) =
        simulate_with(64, 3, configure(&dirs), |sim| async move {
            sim.mesh.set_version(3, wire::VERSION - 1);
            let _handles = start(&sim);
            let primary = primary(&sim).await;
            let standby = 3 - primary;

            let before = sim.node(primary).rt.formats_version().await;
            let refused = sim.node(primary).rt.finalize_formats(&[standby, 3]).await;
            sim.mesh.set_version(3, wire::VERSION);
            let raised = sim.node(standby).rt.finalize_formats(&[primary, 3]).await;
            let again = sim.node(primary).rt.finalize_formats(&[standby, 3]).await;
            settled(&sim).await;
            let mut kept = Vec::new();
            for node in 1..=2 {
                kept.push(sim.node(node).rt.formats_version().await);
            }
            (before, refused, raised, again, kept)
        });

    assert_eq!(before, Ok(FIRST_FORMATS));
    assert_eq!(
        refused,
        Err(FinalizeRefused::Peer {
            node: 3,
            version: Some(wire::VERSION - 1)
        })
    );
    assert_eq!(raised, Ok(Finalized::Raised(wire::VERSION)));
    assert_eq!(again, Ok(Finalized::Already(wire::VERSION)));
    assert_eq!(kept, [Ok(wire::VERSION), Ok(wire::VERSION)]);
}

/// A standby on a build older than the pair's version (one cut off while
/// a newer partner finalized, then rolled back): once it
/// applies the record, from the log or from a snapshot after an outage,
/// its replication stops and says so, and the primary carries on
/// committing without it.
#[test]
fn formats_outrun____standby_older_than_the_pair____its_replication_stops() {
    for from_snapshot in [false, true] {
        let (_root, dirs) = dirs();
        let configure = |node, config: &mut RuntimeConfig| configure(&dirs)(node, config);
        let (outrun, ready, written, applied) = simulate_with(65, 3, configure, |sim| async move {
            sim.mesh.add_path(SERIAL);
            sim.mesh.cut_path(1, 3, SERIAL);
            sim.mesh.cut_path(2, 3, SERIAL);
            let mut witness = WitnessConfig::new([1, 2], 1);
            witness.lease = LEASE;
            witness.margin = MARGIN;
            sim.node(3).rt.serve_witness(witness).unwrap();
            let _handles: Vec<_> = (1..=2)
                .map(|node| {
                    let mut cfg = failover(node);
                    // Every entry is compacted at once: a partner back from
                    // an outage catches up from a snapshot.
                    if from_snapshot {
                        cfg.replication_log_bytes = 0;
                    }
                    sim.node(node).rt.start_failover(cfg).unwrap()
                })
                .collect();
            let p = primary(&sim).await;
            let s = 3 - p;
            let mut outrun = sim.node(s).rt.formats_outrun();
            if from_snapshot {
                isolate(&sim, s);
                pass(4_000).await;
            }

            sim.node(p)
                .rt
                .record_formats(wire::VERSION + 1)
                .await
                .unwrap();
            pass(1_000).await;
            // Compacted past what the standby holds: only a snapshot
            // brings it up to date.
            let compacted = sim.node(p).rt.replication_status().unwrap().base
                > sim.node(s).rt.replication_status().unwrap().last;
            if from_snapshot {
                heal(&sim);
            }
            for _ in 0..200 {
                if outrun.borrow_and_update().is_some() {
                    break;
                }
                pass(50).await;
            }
            pass(4_000).await;
            let written = set(&sim.node(p).spawn_kv(BUCKET), "after", "v").await;
            let status = sim.node(s).rt.replication_status().unwrap();
            let applied = sim.node(p).rt.replication_status().unwrap().commit > status.commit;
            (
                *outrun.borrow(),
                status.ready,
                written,
                applied && (compacted || !from_snapshot),
            )
        });

        assert_eq!(
            outrun,
            Some(wire::VERSION + 1),
            "from a snapshot: {from_snapshot}"
        );
        assert!(!ready, "from a snapshot: {from_snapshot}");
        assert_eq!(written, "ok", "from a snapshot: {from_snapshot}");
        assert!(
            applied,
            "the standby stopped applying (from a snapshot: {from_snapshot})"
        );
    }
}
