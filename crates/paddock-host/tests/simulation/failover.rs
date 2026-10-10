//! Failover of a singleton between an HA pair (nodes 1 and 2) with a
//! witness (node 3) (ROADMAP D10), against the rules spike S16 checked.
//!
//! Every change in each node's `may_run` is logged with its virtual time;
//! `overlap` replays the log and finds any moment when both nodes could run
//! the singleton. The serial line exists only between 1 and 2.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use paddock_host::failover::{
    Arbiter, FailoverConfig, FailoverHandle, Leadership, Standing, WitnessConfig,
};
use tokio::time::Instant;

use crate::harness::{HEARTBEAT, NODE_TIMEOUT, Sim, pass, simulate};

const LEASE: Duration = Duration::from_millis(1000);
const MARGIN: Duration = Duration::from_millis(200);
const SERIAL: &str = "serial";

/// Each change of `may_run`: (node, when, may_run).
type Log = Arc<Mutex<Vec<(u32, Instant, bool)>>>;

struct Pair {
    handles: Vec<FailoverHandle>,
    log: Log,
}

impl Pair {
    fn of(&self, node: u32) -> Leadership {
        self.handles[usize::try_from(node - 1).unwrap()].leadership()
    }

    fn runners(&self) -> Vec<u32> {
        (1..=2).filter(|&n| self.of(n).may_run).collect()
    }

    /// The first moment both nodes could run, if any.
    fn overlap(&self) -> Option<Instant> {
        let log = self.log.lock().unwrap();
        let mut running = [false, false];
        for &(node, at, may_run) in log.iter() {
            running[usize::try_from(node - 1).unwrap()] = may_run;
            if running == [true, true] {
                return Some(at);
            }
        }
        None
    }

    /// When `node` last started running, if it runs now.
    fn running_since(&self, node: u32) -> Option<Instant> {
        let log = self.log.lock().unwrap();
        match log.iter().rev().find(|(n, _, _)| *n == node) {
            Some(&(_, at, true)) => Some(at),
            _ => None,
        }
    }
}

/// Serve the witness on node 3, start failover on 1 and 2, and log `may_run`.
fn start(sim: &Sim, arbiter: Arbiter) -> Pair {
    sim.mesh.add_path(SERIAL);
    sim.mesh.cut_path(1, 3, SERIAL);
    sim.mesh.cut_path(2, 3, SERIAL);
    let mut witness = WitnessConfig::new([1, 2], 1);
    witness.lease = LEASE;
    witness.margin = MARGIN;
    if matches!(arbiter, Arbiter::Witness(_)) {
        sim.node(3).rt.serve_witness(witness).unwrap();
    }
    let log: Log = Arc::default();
    let handles = (1..=2)
        .map(|node| {
            let mut cfg = FailoverConfig::new(3 - node, arbiter);
            cfg.lease = LEASE;
            cfg.margin = MARGIN;
            let handle = sim.node(node).rt.start_failover(cfg).unwrap();
            let mut changes = handle.subscribe();
            let log = log.clone();
            tokio::spawn(async move {
                let mut last = false;
                loop {
                    let may_run = changes.borrow_and_update().may_run;
                    if may_run != last {
                        log.lock().unwrap().push((node, Instant::now(), may_run));
                        last = may_run;
                    }
                    if changes.changed().await.is_err() {
                        // The node's runtime is gone, and its apps with it.
                        if last {
                            log.lock().unwrap().push((node, Instant::now(), false));
                        }
                        return;
                    }
                }
            });
            handle
        })
        .collect();
    Pair { handles, log }
}

/// Cut `node` off from everything: its links stall, its serial line is cut.
fn isolate(sim: &Sim, node: u32) {
    for other in (1..=3).filter(|&o| o != node) {
        sim.mesh.stall(node, other);
        sim.mesh.cut_path(node, other, SERIAL);
    }
}

/// How long a dead primary's singleton may take to move, at most: the
/// standby notices silence, then the witness waits out the lease.
fn takeover_bound() -> Duration {
    NODE_TIMEOUT + LEASE + MARGIN * 2 + HEARTBEAT * 4
}

fn witness() -> Arbiter {
    Arbiter::Witness(3)
}

#[test]
fn start____witness_names_node_1____it_runs_node_2_stands_by() {
    let (one, two) = simulate(41, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;
        (pair.of(1), pair.of(2))
    });

    assert!(one.may_run);
    assert_eq!((one.standing, one.epoch), (Standing::Primary, 1));
    assert_eq!(
        (two.standing, two.epoch, two.may_run),
        (Standing::Standby, 1, false)
    );
}

#[test]
fn failover____primary_cut_off____standby_takes_over_within_the_bound_never_both() {
    let (took, runners, overlap, epoch) = simulate(42, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;

        let cut = Instant::now();
        isolate(&sim, 1);
        pass(5000).await;

        let took = pair.running_since(2).map(|t| t.duration_since(cut));
        (took, pair.runners(), pair.overlap(), pair.of(2).epoch)
    });

    assert_eq!(runners, vec![2]);
    assert_eq!(epoch, 2);
    let took = took.unwrap();
    assert!(
        took <= takeover_bound(),
        "took {took:?}, bound {:?}",
        takeover_bound()
    );
    assert_eq!(overlap, None);
}

#[test]
fn failover____primary_runtime_dropped____standby_takes_over() {
    let (runners, overlap) = simulate(43, 3, |mut sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;

        // Node 1's process is gone: its tasks stop with its runtime.
        let three = sim.nodes.pop().unwrap();
        let two = sim.nodes.pop().unwrap();
        drop(sim.nodes.pop());
        sim.nodes.extend([two, three]);
        pass(5000).await;

        let runners: Vec<u32> = (2..=2).filter(|&n| pair.of(n).may_run).collect();
        (runners, pair.overlap())
    });

    assert_eq!(runners, vec![2]);
    assert_eq!(overlap, None);
}

#[test]
fn failover____network_cut_serial_up____nobody_promoted() {
    let (runners, two, overlap) = simulate(44, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;

        sim.mesh.stall(1, 2);
        pass(6000).await;

        (pair.runners(), pair.of(2), pair.overlap())
    });

    assert_eq!(runners, vec![1]);
    assert_eq!((two.standing, two.epoch), (Standing::Standby, 1));
    assert_eq!(overlap, None);
}

#[test]
fn failover____network_and_serial_cut____primary_renews_at_the_witness_and_keeps_running() {
    let (runners, two, overlap) = simulate(45, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;

        sim.mesh.stall(1, 2);
        sim.mesh.cut_path(1, 2, SERIAL);
        pass(6000).await;

        (pair.runners(), pair.of(2), pair.overlap())
    });

    assert_eq!(runners, vec![1]);
    assert_ne!(two.standing, Standing::Primary);
    assert_eq!(overlap, None);
}

#[test]
fn failover____network_serial_and_primary_wan_cut____hands_over_then_old_primary_rejoins() {
    let (handed, after_heal, overlap) = simulate(46, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;

        sim.mesh.stall(1, 2);
        sim.mesh.cut_path(1, 2, SERIAL);
        sim.mesh.stall(1, 3);
        pass(5000).await;
        let handed = (pair.runners(), pair.of(2).epoch);

        sim.mesh.heal_all();
        sim.mesh.cut_path(1, 3, SERIAL);
        sim.mesh.cut_path(2, 3, SERIAL);
        pass(3000).await;
        let after_heal = (pair.runners(), pair.of(1).standing, pair.of(1).epoch);

        (handed, after_heal, pair.overlap())
    });

    assert_eq!(handed, (vec![2], 2));
    assert_eq!(after_heal, (vec![2], Standing::Standby, 2));
    assert_eq!(overlap, None);
}

#[test]
fn failover____witness_lost_then_primary_lost____no_promotion_until_an_operator_confirms() {
    let (before, after, overlap) = simulate(47, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;
        isolate(&sim, 3);
        pass(500).await;

        isolate(&sim, 1);
        pass(5000).await;
        let before = (pair.runners(), pair.of(2).standing);
        pair.handles[1].confirm();
        pass(3000).await;

        (before, pair.runners(), pair.overlap())
    });

    assert_eq!(before, (vec![], Standing::Standby));
    assert_eq!(after, vec![2]);
    assert_eq!(overlap, None);
}

#[test]
fn paths_arbiter____node_silent_or_network_cut____promotes_only_when_silent_everywhere() {
    let (network_only, everywhere) = simulate(48, 3, |sim| async move {
        let pair = start(&sim, Arbiter::Paths);
        pass(3000).await;

        sim.mesh.stall(1, 2);
        pass(3000).await;
        let network_only = pair.runners();
        sim.mesh.cut_path(1, 2, SERIAL);
        sim.mesh.stall(1, 3);
        pass(3000).await;

        (network_only, pair.of(2))
    });

    assert_eq!(network_only, vec![1]);
    assert_eq!(
        (everywhere.standing, everywhere.may_run),
        (Standing::Primary, true)
    );
}

#[test]
fn operator____partner_silent____awaits_confirmation_then_promotes() {
    let (waiting, promoted) = simulate(49, 3, |sim| async move {
        let pair = start(&sim, Arbiter::Operator);
        pass(3000).await;

        isolate(&sim, 1);
        pass(3000).await;
        let waiting = pair.of(2);
        pair.handles[1].confirm();
        pass(500).await;

        (waiting, pair.of(2))
    });

    assert!(waiting.awaiting_confirmation);
    assert_eq!(waiting.standing, Standing::Standby);
    assert_eq!(
        (promoted.standing, promoted.may_run),
        (Standing::Primary, true)
    );
}

/// Random faults on links, the serial line and the witness for a while,
/// then everything healed: never two runners, and one runner at the end.
#[test]
fn failover____random_faults____never_two_runners_one_after_healing() {
    let seeds = std::env::var("PADDOCK_SIM_FAILOVER_SEEDS")
        .ok()
        .map_or(16, |n| n.parse().unwrap());
    let mut failures = Vec::new();
    for seed in 0..seeds {
        let (overlap, runners, trace) = simulate(seed, 3, move |sim| async move {
            let pair = start(&sim, witness());
            pass(3000).await;
            let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut next = |n: u64| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng % n
            };
            let mut trace = Vec::new();
            let start = Instant::now();
            for _ in 0..30 {
                let (x, y) = [(1, 2), (1, 3), (2, 3)][usize::try_from(next(3)).unwrap()];
                let what = match next(6) {
                    0 => {
                        sim.mesh.stall(x, y);
                        format!("stall {x}-{y}")
                    }
                    1 => {
                        sim.mesh.stall_one_way(x, y);
                        format!("stall {x}->{y}")
                    }
                    2 => {
                        sim.mesh.cut_path(1, 2, SERIAL);
                        "cut serial".to_string()
                    }
                    3 => {
                        sim.mesh.heal_path(1, 2, SERIAL);
                        "heal serial".to_string()
                    }
                    _ => {
                        sim.mesh.heal(x, y);
                        format!("heal {x}-{y}")
                    }
                };
                let wait = 100 + next(2000);
                let states: Vec<String> = (1..=2)
                    .map(|n| {
                        let l = pair.of(n);
                        format!("{n}={:?}/{}/{}", l.standing, l.epoch, l.may_run)
                    })
                    .collect();
                trace.push(format!(
                    "t={} {states:?} {what} then {wait} ms",
                    start.elapsed().as_millis()
                ));
                pass(wait).await;
            }
            sim.mesh.heal_all();
            sim.mesh.cut_path(1, 3, SERIAL);
            sim.mesh.cut_path(2, 3, SERIAL);
            pass(8000).await;
            let log: Vec<String> = pair
                .log
                .lock()
                .unwrap()
                .iter()
                .map(|(n, at, r)| format!("{n}:{}:{r}", at.duration_since(start).as_millis()))
                .collect();
            trace.push(format!("may_run log (ms from start): {log:?}"));
            (pair.overlap(), pair.runners(), trace)
        });
        if overlap.is_some() || runners.len() != 1 {
            failures.push(format!(
                "seed {seed}: overlap {overlap:?}, runners {runners:?}: {trace:?}"
            ));
        }
    }

    assert!(failures.is_empty(), "{failures:#?}");
}

/// Beats the standby sent while still confirming are held by a stall and
/// arrive after it was promoted: they must not renew the old primary.
#[test]
fn held_confirmations____released_after_promotion____do_not_renew_the_old_primary() {
    let overlap = simulate(50, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;

        sim.mesh.stall(1, 2);
        sim.mesh.cut_path(1, 2, SERIAL);
        sim.mesh.stall(1, 3);
        pass(4000).await;
        assert_eq!(pair.runners(), vec![2]);
        sim.mesh.heal(1, 2);
        pass(2000).await;

        pair.overlap()
    });

    assert_eq!(overlap, None);
}
