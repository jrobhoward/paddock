//! Failover of a singleton between an HA pair (nodes 1 and 2) with a
//! witness (node 3), against the rules spike S16 checked.
//!
//! Every change in each node's `may_run` is logged with its virtual time;
//! `overlap` replays the log and finds any moment when both nodes could run
//! the singleton. The serial line exists only between 1 and 2.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use paddock_host::failover::FailoverMsg;
use paddock_host::failover::{
    Arbiter, FailoverConfig, FailoverHandle, Leadership, Standing, SwitchoverRefused,
    WitnessConfig, WitnessPersist,
};
use paddock_host::transport::Frame;
use paddock_host::transport::memory::Faults;
use paddock_host::transport::wire;
use paddock_host::{CallError, LifecycleState};
use tokio::time::Instant;

use crate::common::{body, text};
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
    start_kept(sim, arbiter, None)
}

/// [`start`], with the witness keeping its state through `persist`.
fn start_kept(sim: &Sim, arbiter: Arbiter, persist: Option<WitnessPersist>) -> Pair {
    start_timed(sim, arbiter, persist, LEASE, MARGIN)
}

/// [`start_kept`], with another lease and margin.
fn start_timed(
    sim: &Sim,
    arbiter: Arbiter,
    persist: Option<WitnessPersist>,
    lease: Duration,
    margin: Duration,
) -> Pair {
    sim.mesh.add_path(SERIAL);
    sim.mesh.cut_path(1, 3, SERIAL);
    sim.mesh.cut_path(2, 3, SERIAL);
    let mut witness = WitnessConfig::new([1, 2], 1);
    witness.lease = lease;
    witness.margin = margin;
    if matches!(arbiter, Arbiter::Witness(_)) {
        sim.node(3).rt.serve_witness_kept(witness, persist).unwrap();
    }
    let log: Log = Arc::default();
    let handles = (1..=2)
        .map(|node| {
            let mut cfg = FailoverConfig::new(3 - node, arbiter);
            cfg.lease = lease;
            cfg.margin = margin;
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

/// Every (standing, epoch) `handle` takes from now on, in order.
fn standings(handle: &FailoverHandle) -> Arc<Mutex<Vec<(Standing, u64)>>> {
    let seen: Arc<Mutex<Vec<(Standing, u64)>>> = Arc::default();
    let mut changes = handle.subscribe();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let now = changes.borrow_and_update().clone();
            {
                let mut log = log.lock().unwrap();
                if log.last() != Some(&(now.standing, now.epoch)) {
                    log.push((now.standing, now.epoch));
                }
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    });
    seen
}

fn granted(frame: &Frame) -> bool {
    matches!(frame, Frame::Failover(FailoverMsg::Granted { .. }))
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
fn bound_instances____lease_moves_with_no_gate____guest_code_runs_only_under_the_lease() {
    let (before, standby_before, cut_off, taken_over, old_state) =
        simulate(47, 3, |sim| async move {
            let one = sim.node(1).spawn_bound();
            let two = sim.node(2).spawn_bound();
            let pair = start(&sim, witness());
            pass(3000).await;
            let short = Duration::from_millis(100);
            let before = one.call(text("echo:one"), short).await.map(|e| body(&e));
            let standby_before = two.call(text("echo:two"), short).await;

            isolate(&sim, 1);
            // Past node 1's lease; it hears nobody, so nothing renews it.
            pass(u64::try_from((LEASE + MARGIN).as_millis()).unwrap()).await;
            let cut_off = one.call(text("echo:one"), short).await;
            pass(5000).await;
            let taken_over = two.call(text("echo:two"), short).await.map(|e| body(&e));
            assert_eq!(pair.runners(), vec![2]);
            (before, standby_before, cut_off, taken_over, one.state())
        });

    assert_eq!(before.as_deref(), Ok("one"));
    assert_eq!(standby_before, Err(CallError::Timeout));
    assert_eq!(cut_off, Err(CallError::Timeout));
    assert_eq!(taken_over.as_deref(), Ok("two"));
    assert_eq!(old_state, LifecycleState::Active);
}

/// A standby whose request's answer was lost, and which then hears its
/// primary again, goes back to confirming the primary's lease once the
/// request's retry time passes: the primary keeps running
/// when it later loses the witness.
#[test]
fn lost_answer____partner_heard_again____standby_confirms_the_lease_again() {
    let (runners, overlap) = simulate(48, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;
        let lose_all = Faults {
            drop: 1.0,
            ..Faults::default()
        };

        // The standby hears nothing from the primary and asks; every
        // answer of the witness's to it is lost.
        sim.mesh.set_faults(3, 2, lose_all);
        sim.mesh.stall(1, 2);
        sim.mesh.cut_path(1, 2, SERIAL);
        pass(u64::try_from((NODE_TIMEOUT + HEARTBEAT * 2).as_millis()).unwrap()).await;
        sim.mesh.set_faults(3, 2, Faults::default());
        sim.mesh.heal(1, 2);
        sim.mesh.heal_path(1, 2, SERIAL);
        pass(u64::try_from((LEASE * 2).as_millis()).unwrap()).await;
        // The primary's lease now rests on its partner's confirmations.
        sim.mesh.stall(1, 3);
        pass(u64::try_from((LEASE * 3).as_millis()).unwrap()).await;

        (pair.runners(), pair.overlap())
    });

    assert_eq!(runners, vec![1]);
    assert_eq!(overlap, None);
}

/// A grant whose state cannot be kept is withheld; the standby is still
/// promoted once a later write succeeds, at the epoch that write kept, and
/// never two runners. (That no answer carries an epoch not yet on disk is
/// `witness_tests`'s.)
#[test]
fn witness_write_fails_once____primary_cut_off____standby_promoted_at_the_next_epoch() {
    let kept: Arc<Mutex<Vec<(u64, u32)>>> = Arc::default();
    let (runners, epoch, kept_now, overlap) = simulate(49, 3, |sim| {
        let kept = kept.clone();
        async move {
            let failures = Arc::new(Mutex::new(1));
            let log = kept.clone();
            let persist: WitnessPersist = Arc::new(move |state| {
                let mut failures = failures.lock().unwrap();
                if state.epoch >= 2 && *failures > 0 {
                    *failures -= 1;
                    return Err("disk full".into());
                }
                log.lock().unwrap().push((state.epoch, state.promoted));
                Ok(())
            });
            let pair = start_kept(&sim, witness(), Some(persist));
            pass(3000).await;

            isolate(&sim, 1);
            pass(u64::try_from((takeover_bound() * 3).as_millis()).unwrap()).await;

            let kept_now = kept.lock().unwrap().clone();
            (pair.runners(), pair.of(2).epoch, kept_now, pair.overlap())
        }
    });

    assert_eq!(runners, vec![2]);
    assert_eq!(epoch, 2);
    assert_eq!(kept_now.last(), Some(&(2, 2)));
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

/// An operator's confirmation given while nothing was wrong must not
/// promote the standby on a later, unrelated cut that the witness covers.
#[test]
fn operator_confirm____given_while_healthy_then_network_and_serial_cut____no_second_runner() {
    let (runners, overlap) = simulate(48, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;
        pair.handles[1].confirm();
        pass(3000).await;

        sim.mesh.stall(1, 2);
        sim.mesh.cut_path(1, 2, SERIAL);
        pass(6000).await;

        (pair.runners(), pair.overlap())
    });

    assert_eq!(runners, vec![1]);
    assert_eq!(overlap, None);
}

/// With the witness reachable it decides, so the operator is not asked,
/// and a confirmation sent anyway is ignored.
#[test]
fn operator_confirm____partner_cut_and_witness_reachable____not_asked_and_ignored() {
    let (two, runners, overlap) = simulate(50, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;

        sim.mesh.stall(1, 2);
        sim.mesh.cut_path(1, 2, SERIAL);
        pass(3000).await;
        let two = pair.of(2);
        pair.handles[1].confirm();
        pass(6000).await;

        (two, pair.runners(), pair.overlap())
    });

    assert!(!two.awaiting_confirmation);
    assert_eq!(two.standing, Standing::Standby);
    assert_eq!(runners, vec![1]);
    assert_eq!(overlap, None);
}

/// The witness's answers to the standby take longer than a lease (but
/// less than the node timeout) for a while, so its grant arrives after a
/// later request left. The lease runs from the granted request and is over
/// on arrival: the new primary runs only once a renewal comes back, not at
/// once as it would with a lease from the later request.
#[test]
fn failover____grant_arrives_after_a_retry____lease_runs_from_the_granted_request() {
    let lease = Duration::from_millis(400);
    let (runs_when_promoted, two, overlap) = simulate(51, 3, |sim| async move {
        let pair = start_timed(&sim, witness(), None, lease, Duration::from_millis(100));
        let mut changes = pair.handles[1].subscribe();
        pass(3000).await;

        let slow = Faults {
            delay: Duration::from_millis(450),
            ..Faults::default()
        };
        sim.mesh.set_faults(3, 2, slow);
        isolate(&sim, 1);
        while changes.borrow_and_update().standing != Standing::Primary {
            changes.changed().await.unwrap();
        }
        let runs_when_promoted = changes.borrow().may_run;
        sim.mesh.set_faults(3, 2, Faults::default());
        pass(3000).await;

        (runs_when_promoted, pair.of(2), pair.overlap())
    });

    assert_eq!(
        (two.standing, two.epoch, two.may_run),
        (Standing::Primary, 2, true)
    );
    assert!(!runs_when_promoted);
    assert_eq!(overlap, None);
}

/// The witness's grant to the standby is lost: its next request, at its
/// old epoch, is refused, and it steps down, then resumes as the node the
/// witness promoted.
#[test]
fn lost_grant____standby_asks_again____refused_steps_down_then_resumes() {
    let (two, path, overlap) = simulate(52, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;
        let path = standings(&pair.handles[1]);

        sim.mesh.lose_next(3, 2, granted);
        isolate(&sim, 1);
        pass(8000).await;

        let path = path.lock().unwrap().clone();
        (pair.of(2), path, pair.overlap())
    });

    assert_eq!(
        (two.standing, two.epoch, two.may_run),
        (Standing::Primary, 2, true)
    );
    assert_eq!(
        path,
        vec![
            (Standing::Standby, 1),
            (Standing::Down, 2),
            (Standing::Primary, 2)
        ]
    );
    assert_eq!(overlap, None);
}

/// After a lost grant, the old primary comes back and is fenced by the
/// witness (`Down`) while the standby cannot reach the witness: the
/// standby stands down after hearing its partner `Down` for a lease, the
/// old primary's resume is refused, and once the witness is reachable the
/// node it promoted resumes.
#[test]
fn partner_down____for_a_lease____standby_stands_down_and_the_promoted_node_resumes() {
    let (one_path, two_path, one, two, overlap) = simulate(53, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;
        let one_path = standings(&pair.handles[0]);
        let two_path = standings(&pair.handles[1]);

        sim.mesh.lose_next(3, 2, granted);
        isolate(&sim, 1);
        // Long enough for the grant, not for the request after it.
        pass(2000).await;
        sim.mesh.stall_one_way(2, 3);
        sim.mesh.heal(1, 2);
        sim.mesh.heal(1, 3);
        sim.mesh.heal_path(1, 2, SERIAL);
        pass(5000).await;
        sim.mesh.heal(2, 3);
        pass(5000).await;

        let one_path = one_path.lock().unwrap().clone();
        let two_path = two_path.lock().unwrap().clone();
        (one_path, two_path, pair.of(1), pair.of(2), pair.overlap())
    });

    assert_eq!(one_path[..2], [(Standing::Primary, 1), (Standing::Down, 2)]);
    assert!(
        two_path
            .windows(2)
            .any(|w| w == [(Standing::Standby, 1), (Standing::Down, 1)]),
        "{two_path:?}"
    );
    assert_eq!(
        (two.standing, two.epoch, two.may_run),
        (Standing::Primary, 2, true)
    );
    assert_eq!((one.standing, one.epoch), (Standing::Standby, 2));
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
        let two = pair.of(2);
        let before = (pair.runners(), two.standing, two.awaiting_confirmation);
        pair.handles[1].confirm();
        pass(3000).await;

        (before, pair.runners(), pair.overlap())
    });

    assert_eq!(before, (vec![], Standing::Standby, true));
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
/// `PADDOCK_SIM_FAILOVER_SEEDS` seeds (default 16) from
/// `PADDOCK_SIM_FAILOVER_FIRST_SEED` (default 0).
#[test]
fn failover____random_faults____never_two_runners_one_after_healing() {
    random_faults(false);
}

/// As [`failover____random_faults____never_two_runners_one_after_healing`],
/// with planned switchovers among the faults.
#[test]
fn failover____random_faults_and_switchovers____never_two_runners_one_after_healing() {
    random_faults(true);
}

/// Random faults on links, the serial line and the witness for a while,
/// and with `switchovers` a planned switchover now and then, then
/// everything healed: never two runners, and one runner at the end.
/// `PADDOCK_SIM_FAILOVER_SEEDS` seeds (default 16) from
/// `PADDOCK_SIM_FAILOVER_FIRST_SEED` (default 0).
fn random_faults(switchovers: bool) {
    let number = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .map_or(default, |n| n.parse().unwrap())
    };
    let first = number("PADDOCK_SIM_FAILOVER_FIRST_SEED", 0);
    let seeds = number("PADDOCK_SIM_FAILOVER_SEEDS", 16);
    let mut failures = Vec::new();
    for seed in first..first + seeds {
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
                let what = match next(if switchovers { 8 } else { 6 }) {
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
                    6 | 7 => {
                        let mut said = Vec::new();
                        for n in 1..=2 {
                            let r = pair.handles[usize::try_from(n - 1).unwrap()]
                                .switchover()
                                .await;
                            said.push(format!("{n}:{r:?}"));
                        }
                        format!("switchover {said:?}")
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

/// A planned switchover: the standby runs the singleton
/// within a lease and its margin, without a node timeout, the old primary
/// rejoins as its standby, and the two never run at once. A standby asked
/// to hand over refuses.
#[test]
fn switchover____healthy_pair____standby_takes_over_within_a_lease_old_primary_rejoins() {
    let (refused, accepted, took, one, two, overlap) = simulate(51, 3, |sim| async move {
        let pair = start(&sim, witness());
        pass(3000).await;

        let refused = pair.handles[1].switchover().await;
        let asked = Instant::now();
        let accepted = pair.handles[0].switchover().await;
        pass(4000).await;

        let took = pair.running_since(2).map(|t| t.duration_since(asked));
        (
            refused,
            accepted,
            took,
            pair.of(1),
            pair.of(2),
            pair.overlap(),
        )
    });

    assert_eq!(refused, Err(SwitchoverRefused::NotPrimary));
    assert_eq!(accepted, Ok(()));
    let took = took.unwrap();
    let bound = LEASE + MARGIN * 2 + HEARTBEAT * 4;
    assert!(took <= bound, "took {took:?}, bound {bound:?}");
    assert!(took < takeover_bound(), "no node timeout: {took:?}");
    assert_eq!(
        (two.standing, two.epoch, two.may_run),
        (Standing::Primary, 2, true)
    );
    assert_eq!(
        (one.standing, one.epoch, one.may_run),
        (Standing::Standby, 2, false)
    );
    assert_eq!(overlap, None);
}

/// A planned switchover with the witness one release older (DESIGN.md,
/// "Mixed versions during a release"): the old primary's release is a
/// frame the witness cannot read, so it is not sent, and the witness waits
/// the lease out as for a release lost; with a witness of this build it
/// grants within a few beats.
#[test]
fn switchover____witness_one_release_older____standby_waits_the_lease_out() {
    let switch = |witness_version: u16| {
        simulate(52, 3, move |sim| async move {
            sim.mesh.set_version(3, witness_version);
            let pair = start(&sim, witness());
            pass(3000).await;

            let asked = Instant::now();
            let accepted = pair.handles[0].switchover().await;
            pass(4000).await;

            let took = pair.running_since(2).map(|t| t.duration_since(asked));
            (accepted, took, pair.of(2), pair.overlap())
        })
    };

    let (old_accepted, old_took, old_two, old_overlap) = switch(wire::VERSION - 1);
    let (new_accepted, new_took, _, new_overlap) = switch(wire::VERSION);

    assert_eq!((old_accepted, new_accepted), (Ok(()), Ok(())));
    let (old_took, new_took) = (old_took.unwrap(), new_took.unwrap());
    assert!(old_took >= LEASE, "older witness: {old_took:?}");
    assert!(
        old_took <= LEASE + MARGIN * 2 + HEARTBEAT * 4,
        "{old_took:?}"
    );
    assert!(new_took < LEASE, "this build's witness: {new_took:?}");
    assert_eq!(
        (old_two.standing, old_two.epoch, old_two.may_run),
        (Standing::Primary, 2, true)
    );
    assert_eq!((old_overlap, new_overlap), (None, None));
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
