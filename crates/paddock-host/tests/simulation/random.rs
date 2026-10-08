//! Random scenarios: from a seed, a sequence of spawns, links, unlinks,
//! monitors, demonitors, kills, calls, casts, partitions, heals and pauses
//! across three nodes with random frame delays, then a check of every live
//! instance's exit and down notices, and every call's result, against the
//! distribution rules (DESIGN.md, "Distribution").
//!
//! `PADDOCK_SIM_SEEDS` sets how many seeds each test runs (default 24),
//! starting from `PADDOCK_SIM_FIRST_SEED` (default 0); `PADDOCK_SIM_SEED`
//! runs only that seed. A failure prints the seed, the
//! operations and the notices, and the seed replays it exactly.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::Duration;

use paddock_host::InstanceHandle;
use paddock_host::transport::memory::Faults;

use crate::common::{ask, pid_text};
use crate::harness::{Sim, pass, simulate};

const NODES: u32 = 3;
const OPS: usize = 40;
const MAX_INSTANCES: usize = 12;

/// SplitMix64, for choosing operations.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())])
    }
}

struct Inst {
    handle: InstanceHandle,
    node: u32,
    pid: String,
    /// The operation that killed it.
    killed: Option<usize>,
}

/// Read through `Debug`, in traces.
#[derive(Debug)]
#[allow(dead_code)]
enum Op {
    Spawn(usize),
    Link {
        a: usize,
        b: usize,
    },
    /// Asked of `a`.
    Unlink {
        a: usize,
        b: usize,
    },
    Monitor {
        a: usize,
        b: usize,
        reference: String,
    },
    Demonitor {
        a: usize,
        reference: String,
    },
    Kill(usize),
    Call {
        a: usize,
        b: usize,
        result: String,
    },
    Cast {
        a: usize,
        b: usize,
    },
    Partition(u32, u32),
    Heal(u32, u32),
    Pass(u64),
}

struct Link {
    op: usize,
    a: usize,
    b: usize,
    unlinked: bool,
}

struct Monitor {
    op: usize,
    watcher: usize,
    target: usize,
    reference: String,
    demonitored: bool,
}

/// What a scenario did and what its instances saw.
struct Run {
    seed: u64,
    drops: bool,
    insts: Vec<Inst>,
    ops: Vec<Op>,
    /// Pairs cut now.
    cut: BTreeSet<(u32, u32)>,
    /// Pairs cut when each operation ran.
    cut_before: Vec<BTreeSet<(u32, u32)>>,
    links: Vec<Link>,
    monitors: Vec<Monitor>,
    /// Each live instance's notices at the end.
    infos: BTreeMap<usize, String>,
}

fn pair(x: u32, y: u32) -> (u32, u32) {
    (x.min(y), x.max(y))
}

impl Run {
    fn alive(&self) -> Vec<usize> {
        (0..self.insts.len())
            .filter(|&i| self.insts[i].killed.is_none())
            .collect()
    }

    fn node(&self, i: usize) -> u32 {
        self.insts[i].node
    }

    /// Whether `x` and `y` were cut when operation `op` ran, or are cut
    /// by a later one.
    fn cut_from(&self, x: u32, y: u32, op: usize) -> bool {
        x != y
            && (self.cut_before[op].contains(&pair(x, y))
                || self.ops[op + 1..]
                    .iter()
                    .any(|o| matches!(o, Op::Partition(p, q) if pair(*p, *q) == pair(x, y))))
    }

    fn cut_at(&self, x: u32, y: u32, op: usize) -> bool {
        self.cut_before[op].contains(&pair(x, y))
    }

    fn killed_before(&self, i: usize, op: usize) -> bool {
        self.insts[i].killed.is_some_and(|k| k < op)
    }

    /// The operations and notices, for reproducing a failure and for
    /// comparing two runs of a seed.
    fn trace(&self) -> String {
        let mut out = String::new();
        for (i, inst) in self.insts.iter().enumerate() {
            writeln!(out, "instance {i} = {}", inst.pid).unwrap();
        }
        for (i, op) in self.ops.iter().enumerate() {
            writeln!(out, "op {i}: {op:?}").unwrap();
        }
        for (i, infos) in &self.infos {
            writeln!(out, "notices of {i} ({}): {infos}", self.insts[*i].pid).unwrap();
        }
        out
    }
}

async fn scenario(sim: Sim, seed: u64, drops: bool) -> Run {
    let mut rng = Rng(seed ^ 0x5EED_5EED);
    let faults = Faults {
        delay: Duration::from_millis(u64::try_from(rng.below(11)).unwrap()),
        jitter: Duration::from_millis(u64::try_from(rng.below(21)).unwrap()),
        drop: if drops { 0.05 } else { 0.0 },
    };
    sim.mesh.set_default_faults(faults);
    let mut run = Run {
        seed,
        drops,
        insts: Vec::new(),
        ops: Vec::new(),
        cut: BTreeSet::new(),
        cut_before: Vec::new(),
        links: Vec::new(),
        monitors: Vec::new(),
        infos: BTreeMap::new(),
    };
    for node in 1..=NODES {
        run.spawn(&sim, node);
        run.spawn(&sim, node);
    }

    for index in 0..OPS {
        run.cut_before.push(run.cut.clone());
        let op = match run.step(&sim, &mut rng, index).await {
            Some(op) => op,
            None => {
                let ms = u64::try_from(rng.below(31)).unwrap();
                pass(ms).await;
                Op::Pass(ms)
            }
        };
        run.ops.push(op);
    }

    pass(1000).await;
    for i in run.alive() {
        let infos = ask(&run.insts[i].handle, "infos").await;
        run.infos.insert(i, infos);
    }
    run
}

impl Run {
    fn spawn(&mut self, sim: &Sim, node: u32) -> usize {
        let handle = sim.node(node).spawn("trap-exits");
        let pid = pid_text(handle.pid());
        self.insts.push(Inst {
            handle,
            node,
            pid,
            killed: None,
        });
        self.insts.len() - 1
    }

    /// Ask instance `i` `command`.
    async fn ask(&self, i: usize, command: &str) -> String {
        ask(&self.insts[i].handle, command).await
    }

    /// Choose and perform operation `index`; `None` if the one chosen does
    /// not apply now.
    async fn step(&mut self, sim: &Sim, rng: &mut Rng, index: usize) -> Option<Op> {
        let alive = self.alive();
        let a = rng.pick(&alive).unwrap();
        let others: Vec<usize> = (0..self.insts.len()).filter(|&i| i != a).collect();
        let b = rng.pick(&others).unwrap();
        match rng.below(100) {
            0..15 => self.link(index, a, b).await,
            15..20 => self.unlink(rng, &alive).await,
            20..35 => Some(self.monitor(index, a, b).await),
            35..40 => self.demonitor(rng, &alive).await,
            40..48 if alive.len() > 4 => {
                self.insts[a].handle.kill();
                self.insts[a].killed = Some(index);
                Some(Op::Kill(a))
            }
            48..58 => {
                let pid = &self.insts[b].pid;
                let result = self.ask(a, &format!("call:{pid}:200:echo:x")).await;
                Some(Op::Call { a, b, result })
            }
            58..68 => {
                self.ask(a, &format!("cast:{}:count", self.insts[b].pid))
                    .await;
                Some(Op::Cast { a, b })
            }
            68..74 => self.partition(sim, rng),
            74..82 => self.heal(sim, rng),
            82..85 if self.insts.len() < MAX_INSTANCES => {
                let node = u32::try_from(rng.below(3)).unwrap() + 1;
                Some(Op::Spawn(self.spawn(sim, node)))
            }
            _ => None,
        }
    }

    /// Link `a` to `b`, unless the pair was ever linked.
    async fn link(&mut self, index: usize, a: usize, b: usize) -> Option<Op> {
        if self
            .links
            .iter()
            .any(|l| pair_of(l.a, l.b) == pair_of(a, b))
        {
            return None;
        }
        self.ask(a, &format!("link:{}", self.insts[b].pid)).await;
        self.links.push(Link {
            op: index,
            a,
            b,
            unlinked: false,
        });
        Some(Op::Link { a, b })
    }

    /// Unlink a link with a live end, asked of that end.
    async fn unlink(&mut self, rng: &mut Rng, alive: &[usize]) -> Option<Op> {
        let open: Vec<usize> = (0..self.links.len())
            .filter(|&l| !self.links[l].unlinked)
            .filter(|&l| alive.contains(&self.links[l].a) || alive.contains(&self.links[l].b))
            .collect();
        let l = rng.pick(&open)?;
        let link = &self.links[l];
        let (asker, other) = if alive.contains(&link.a) {
            (link.a, link.b)
        } else {
            (link.b, link.a)
        };
        self.ask(asker, &format!("unlink:{}", self.insts[other].pid))
            .await;
        self.links[l].unlinked = true;
        Some(Op::Unlink { a: asker, b: other })
    }

    async fn monitor(&mut self, index: usize, a: usize, b: usize) -> Op {
        let reference = self.ask(a, &format!("monitor:{}", self.insts[b].pid)).await;
        self.monitors.push(Monitor {
            op: index,
            watcher: a,
            target: b,
            reference: reference.clone(),
            demonitored: false,
        });
        Op::Monitor { a, b, reference }
    }

    /// Remove a monitor whose watcher is alive.
    async fn demonitor(&mut self, rng: &mut Rng, alive: &[usize]) -> Option<Op> {
        let open: Vec<usize> = (0..self.monitors.len())
            .filter(|&m| !self.monitors[m].demonitored)
            .filter(|&m| alive.contains(&self.monitors[m].watcher))
            .collect();
        let m = rng.pick(&open)?;
        let (watcher, reference) = (self.monitors[m].watcher, self.monitors[m].reference.clone());
        self.ask(watcher, &format!("demonitor:{reference}")).await;
        self.monitors[m].demonitored = true;
        Some(Op::Demonitor {
            a: watcher,
            reference,
        })
    }

    fn partition(&mut self, sim: &Sim, rng: &mut Rng) -> Option<Op> {
        let whole: Vec<(u32, u32)> = (1..=NODES)
            .flat_map(|x| (x + 1..=NODES).map(move |y| (x, y)))
            .filter(|p| !self.cut.contains(p))
            .collect();
        let (x, y) = rng.pick(&whole)?;
        sim.mesh.partition(x, y);
        self.cut.insert((x, y));
        Some(Op::Partition(x, y))
    }

    fn heal(&mut self, sim: &Sim, rng: &mut Rng) -> Option<Op> {
        let broken: Vec<(u32, u32)> = self.cut.iter().copied().collect();
        let (x, y) = rng.pick(&broken)?;
        sim.mesh.heal(x, y);
        self.cut.remove(&(x, y));
        Some(Op::Heal(x, y))
    }
}

fn pair_of(a: usize, b: usize) -> (usize, usize) {
    (a.min(b), a.max(b))
}

/// One instance's notices: exit reasons by pid, and downs (pid, reason) by
/// reference.
struct Notices<'a> {
    exits: BTreeMap<&'a str, Vec<&'a str>>,
    downs: BTreeMap<&'a str, Vec<(&'a str, &'a str)>>,
}

/// Every rule a run broke.
fn violations(run: &Run) -> Vec<String> {
    let mut out = Vec::new();
    for (&e, infos) in &run.infos {
        let notices = parse(&mut out, e, infos);
        check_links(run, e, &notices, &mut out);
        check_monitors(run, e, &notices, &mut out);
    }
    check_calls(run, &mut out);
    out
}

fn parse<'a>(out: &mut Vec<String>, e: usize, infos: &'a str) -> Notices<'a> {
    let mut notices = Notices {
        exits: BTreeMap::new(),
        downs: BTreeMap::new(),
    };
    for entry in infos.split(';').filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = entry.split(':').collect();
        match parts.as_slice() {
            ["exit", pid, reason] => notices.exits.entry(pid).or_default().push(reason),
            ["down", reference, pid, reason] => {
                notices
                    .downs
                    .entry(reference)
                    .or_default()
                    .push((pid, reason));
            }
            _ => out.push(format!("{e}: unexpected notice {entry}")),
        }
    }
    notices
}

/// At most one exit per link, from a linked pid, and one when the rules say
/// so.
fn check_links(run: &Run, e: usize, notices: &Notices<'_>, out: &mut Vec<String>) {
    let mine: Vec<&Link> = run.links.iter().filter(|l| l.a == e || l.b == e).collect();
    let partner = |l: &Link| if l.a == e { l.b } else { l.a };
    for pid in notices.exits.keys() {
        if !mine.iter().any(|l| run.insts[partner(l)].pid == *pid) {
            out.push(format!("{e}: exit from {pid}, never linked"));
        }
    }
    for link in mine {
        let (o, initiator) = (partner(link), link.a == e);
        let seen = notices
            .exits
            .get(run.insts[o].pid.as_str())
            .cloned()
            .unwrap_or_default();
        let o_killed = run.insts[o].killed.is_some();
        let cut = run.cut_from(run.node(e), run.node(o), link.op);
        let mut allowed = Vec::new();
        if o_killed {
            allowed.push("killed");
        }
        if o_killed && initiator {
            allowed.push("noproc");
        }
        if cut {
            allowed.push("noconnection");
        }
        // The target's side exists only once the link frame arrives, which a
        // cut can prevent.
        let due = if initiator {
            o_killed || cut
        } else {
            o_killed && !cut
        };
        let must = due && !link.unlinked && !run.drops;
        let what = format!("link {e}-{o} (op {})", link.op);
        check(out, e, &what, &seen, &allowed, must);
    }
}

/// At most one down per reference, naming the target, and one when due.
fn check_monitors(run: &Run, e: usize, notices: &Notices<'_>, out: &mut Vec<String>) {
    let mine: Vec<&Monitor> = run.monitors.iter().filter(|m| m.watcher == e).collect();
    for (reference, seen) in &notices.downs {
        let Some(m) = mine.iter().find(|m| m.reference == *reference) else {
            out.push(format!("{e}: down for unknown reference {reference}"));
            continue;
        };
        for (pid, _) in seen {
            if *pid != run.insts[m.target].pid {
                out.push(format!("{e}: down {reference} names {pid}"));
            }
        }
    }
    for m in mine {
        let seen: Vec<&str> = notices
            .downs
            .get(m.reference.as_str())
            .map(|v| v.iter().map(|(_, r)| *r).collect())
            .unwrap_or_default();
        let t_killed = run.insts[m.target].killed.is_some();
        let cut = run.cut_from(run.node(e), run.node(m.target), m.op);
        let mut allowed = Vec::new();
        if t_killed {
            allowed.extend(["killed", "noproc"]);
        }
        if cut {
            allowed.push("noconnection");
        }
        let must = !m.demonitored && !run.drops && (t_killed || cut);
        let what = format!("monitor {} (op {})", m.reference, m.op);
        check(out, e, &what, &seen, &allowed, must);
    }
}

/// A call's result follows from the state when it was made.
fn check_calls(run: &Run, out: &mut Vec<String>) {
    for (i, op) in run.ops.iter().enumerate() {
        let Op::Call { a, b, result } = op else {
            continue;
        };
        // A kill is a signal: a call made in the same instant can still
        // reach the target, which then dies with it (`exited`).
        let expected: &[&str] = if run.cut_at(run.node(*a), run.node(*b), i) {
            &["err:noconnection"]
        } else if run.killed_before(*b, i) {
            &["err:noproc", "err:exited:killed"]
        } else {
            &["ok:x"]
        };
        let lost = run.drops && result == "err:timeout";
        if !expected.contains(&result.as_str()) && !lost {
            out.push(format!(
                "op {i}: call {a}->{b} gave {result}, expected {expected:?}"
            ));
        }
    }
}

/// One link's exits or one monitor's downs: none outside `allowed`, at most
/// one, and exactly one if `must`.
fn check(out: &mut Vec<String>, e: usize, what: &str, seen: &[&str], allowed: &[&str], must: bool) {
    if seen.len() > 1 {
        out.push(format!("{e}: {what}: {} notices {seen:?}", seen.len()));
    }
    for reason in seen {
        if !allowed.contains(reason) {
            out.push(format!("{e}: {what}: reason {reason}, allowed {allowed:?}"));
        }
    }
    if must && seen.is_empty() {
        out.push(format!("{e}: {what}: no notice, one due ({allowed:?})"));
    }
}

fn run_seed(seed: u64, drops: bool) -> Run {
    simulate(seed, NODES, move |sim| scenario(sim, seed, drops))
}

/// The seeds to run: `PADDOCK_SIM_SEED`, or `PADDOCK_SIM_SEEDS` of them
/// from `PADDOCK_SIM_FIRST_SEED`.
fn seeds() -> Vec<u64> {
    if let Ok(seed) = std::env::var("PADDOCK_SIM_SEED") {
        return vec![seed.parse().expect("PADDOCK_SIM_SEED is a number")];
    }
    let number = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .map_or(default, |n| n.parse().expect("a number"))
    };
    let first = number("PADDOCK_SIM_FIRST_SEED", 0);
    (first..first + number("PADDOCK_SIM_SEEDS", 24)).collect()
}

/// Run every seed, several at once, and fail with every broken seed's trace.
fn run_all(drops: bool) {
    let seeds = seeds();
    let threads = std::thread::available_parallelism().map_or(4, usize::from);
    let failures: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                let mine: Vec<u64> = seeds.iter().copied().skip(t).step_by(threads).collect();
                scope.spawn(move || {
                    mine.into_iter()
                        .filter_map(|seed| {
                            let run = run_seed(seed, drops);
                            let broken = violations(&run);
                            (!broken.is_empty()).then(|| {
                                format!(
                                    "seed {} (drops: {}):\n  {}\n{}",
                                    run.seed,
                                    run.drops,
                                    broken.join("\n  "),
                                    run.trace()
                                )
                            })
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect()
    });
    assert!(
        failures.is_empty(),
        "{} of {} seeds broke a rule; rerun one with PADDOCK_SIM_SEED=<seed>\n\n{}",
        failures.len(),
        seeds.len(),
        failures.join("\n")
    );
}

#[test]
fn random____reliable_frames____every_rule_holds() {
    run_all(false);
}

#[test]
fn random____lost_frames____safety_rules_hold() {
    run_all(true);
}

#[test]
fn random____same_seed____same_trace() {
    for seed in [11, 12] {
        let first = run_seed(seed, false).trace();
        let second = run_seed(seed, false).trace();

        assert_eq!(first, second, "seed {seed}");
    }
}
