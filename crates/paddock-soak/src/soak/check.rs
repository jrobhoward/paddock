//! The failover check: no moment when both nodes of the pair ran the
//! singleton, and exactly one running it after each heal.
//!
//! A node runs the singleton from its gate's `singleton starting` record
//! (written before the app's `init`) until `singleton killed` or `cannot
//! start singleton`, or until its process is killed, exits or is started
//! again. Time while the process is stopped (`SIGSTOP`) does not count: it
//! runs nothing then.
//!
//! A singleton instance stops running guest code when its node's lease
//! ends, by itself, before the gate kills it. A run therefore ends no later
//! than the lease: at the end a node logs when it notices it late
//! (`failover lease ended`, `ended_ms_ago`), or else at the bound its
//! `failover lease extended` records give (`until_ms` plus `slack_ms` from
//! the last one), which holds even when the process is killed before it
//! can log anything more. A callback cut off after the lease's end
//! (`crashed: lease_ended`) extends the run to that record. Without lease
//! records a run lasts until the gate's kill. Every boundary is taken on the
//! side that makes a run longer, so the check errs toward reporting an
//! overlap.

use std::fmt::Write as _;
use std::path::Path;

use super::events::{self, Event, What};
use super::time::parse_rfc3339_us;

/// A boundary in one node's running of the singleton.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    /// The gate is starting it.
    Start,
    /// The gate killed it, or could not start it.
    Stop,
    /// The process was started.
    Spawn,
    /// The process was killed or exited.
    Gone,
    /// The process was stopped.
    Freeze,
    /// The process is about to be continued.
    Thaw,
    /// The node's lease ended (logged late, dated back to the end).
    LeaseEnded,
    /// A callback of the singleton was cut off at the lease's end.
    CutOff,
    /// The node's lease ends no later than this time.
    LeaseBound(i64),
}

/// A mark at a time, on a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Record {
    pub at_us: i64,
    pub node: u32,
    pub mark: Mark,
}

/// A half-open time span `[start, end)` in microseconds.
pub(crate) type Span = (i64, i64);

/// The gate's and the instances' records for `app` in a node's log, and
/// the node's late lease ends.
pub(crate) fn log_records(node: u32, app: &str, text: &str) -> Vec<Record> {
    let field = format!("app={app}");
    text.lines()
        .filter_map(|line| {
            let line = strip_ansi(line);
            let at_us = parse_rfc3339_us(line.split_whitespace().next()?)?;
            if line.contains("failover lease extended") {
                let field = |name: &str| -> Option<i64> {
                    let value = line
                        .split_whitespace()
                        .find_map(|w| w.strip_prefix(name)?.strip_prefix('='))?;
                    let digits = value
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(value.len());
                    value[..digits].parse().ok()
                };
                let ms = field("until_ms")? + field("slack_ms")?;
                return Some(Record {
                    at_us,
                    node,
                    mark: Mark::LeaseBound(at_us + ms * 1_000),
                });
            }
            if line.contains("failover lease ended") {
                let value = line
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("ended_ms_ago="))?;
                let digits = value
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(value.len());
                let ago_ms: i64 = value[..digits].parse().ok()?;
                return Some(Record {
                    at_us: at_us - ago_ms * 1_000,
                    node,
                    mark: Mark::LeaseEnded,
                });
            }
            // An event field (`app=x`) or the instance span's (`instance{app=x`).
            if !line
                .split(|c: char| c.is_whitespace() || c == '{')
                .any(|w| w == field)
            {
                return None;
            }
            let mark = if line.contains("lease_ended") {
                Mark::CutOff
            } else if line.contains("singleton starting") {
                Mark::Start
            } else if line.contains("singleton killed") || line.contains("cannot start singleton") {
                Mark::Stop
            } else {
                return None;
            };
            Some(Record { at_us, node, mark })
        })
        .collect()
}

/// The runner's process signals, as marks.
pub(crate) fn event_records(events: &[Event]) -> Vec<Record> {
    events
        .iter()
        .filter_map(|e| {
            let mark = match e.what {
                What::Spawn => Mark::Spawn,
                What::Kill | What::Exited => Mark::Gone,
                What::Freeze => Mark::Freeze,
                What::Thaw => Mark::Thaw,
                _ => return None,
            };
            Some(Record {
                at_us: e.at_us,
                node: e.node,
                mark,
            })
        })
        .collect()
}

/// When `node` ran the singleton, up to `horizon` for a run still open.
pub(crate) fn running(records: &[Record], node: u32, horizon: i64) -> Vec<Span> {
    let mut mine: Vec<Record> = records.iter().copied().filter(|r| r.node == node).collect();
    mine.sort_by_key(|r| r.at_us);
    let (mut runs, mut frozen): (Vec<Span>, Vec<Span>) = (Vec::new(), Vec::new());
    let (mut run, mut freeze): (Option<i64>, Option<i64>) = (None, None);
    // The run a lease end closed, which a cut-off callback extends.
    let mut closed_by_lease: Option<usize> = None;
    // The latest bound on the open run's lease.
    let mut bound: Option<i64> = None;
    // Close the open run at `end`, or at its lease's bound if earlier.
    let close = |runs: &mut Vec<Span>,
                 run: &mut Option<i64>,
                 bound: &mut Option<i64>,
                 closed_by_lease: &mut Option<usize>,
                 end: i64| {
        if let Some(s) = run.take() {
            match bound.take() {
                Some(b) if b < end => {
                    runs.push((s, b.max(s)));
                    *closed_by_lease = Some(runs.len() - 1);
                }
                _ => runs.push((s, end)),
            }
        }
    };
    for r in mine {
        match r.mark {
            Mark::Start => {
                run.get_or_insert(r.at_us);
                closed_by_lease = None;
            }
            Mark::LeaseBound(b) => {
                if run.is_some() {
                    bound = Some(b);
                }
            }
            Mark::Stop => close(
                &mut runs,
                &mut run,
                &mut bound,
                &mut closed_by_lease,
                r.at_us,
            ),
            Mark::LeaseEnded => {
                if let Some(s) = run.take() {
                    runs.push((s, r.at_us.max(s)));
                    closed_by_lease = Some(runs.len() - 1);
                }
                bound = None;
            }
            Mark::CutOff => {
                if let Some(span) = closed_by_lease.and_then(|i| runs.get_mut(i)) {
                    span.1 = span.1.max(r.at_us);
                }
            }
            Mark::Spawn | Mark::Gone => {
                close(
                    &mut runs,
                    &mut run,
                    &mut bound,
                    &mut closed_by_lease,
                    r.at_us,
                );
                frozen.extend(freeze.take().map(|s| (s, r.at_us)));
                if r.mark == Mark::Spawn {
                    closed_by_lease = None;
                }
            }
            Mark::Freeze => {
                freeze.get_or_insert(r.at_us);
            }
            Mark::Thaw => frozen.extend(freeze.take().map(|s| (s, r.at_us))),
        }
    }
    close(
        &mut runs,
        &mut run,
        &mut bound,
        &mut closed_by_lease,
        horizon,
    );
    frozen.extend(freeze.map(|s| (s, horizon)));
    subtract(&runs, &frozen)
}

/// `spans` with every part inside one of `holes` removed.
pub(crate) fn subtract(spans: &[Span], holes: &[Span]) -> Vec<Span> {
    let mut holes = holes.to_vec();
    holes.sort_unstable();
    let mut out = Vec::new();
    for &(start, end) in spans {
        let mut from = start;
        for &(h_start, h_end) in &holes {
            if h_end <= from || h_start >= end {
                continue;
            }
            if h_start > from {
                out.push((from, h_start));
            }
            from = from.max(h_end);
        }
        if from < end {
            out.push((from, end));
        }
    }
    out
}

/// Where spans in `a` and `b` overlap, each of positive length.
pub(crate) fn overlaps(a: &[Span], b: &[Span]) -> Vec<Span> {
    let mut out = Vec::new();
    for &(a_start, a_end) in a {
        for &(b_start, b_end) in b {
            let (start, end) = (a_start.max(b_start), a_end.min(b_end));
            if start < end {
                out.push((start, end));
            }
        }
    }
    out.sort_unstable();
    out
}

/// What the check found.
#[derive(Debug)]
pub(crate) struct Report {
    /// Moments when both nodes ran the singleton.
    pub overlaps: Vec<Span>,
    /// Heals after which other than one node ran it: when, and which ran.
    pub unsettled: Vec<(i64, Vec<u32>)>,
    /// Starts read from the logs; none means nothing was checked.
    pub starts: usize,
    /// The first event's time, which the report counts from.
    pub origin_us: i64,
    /// The events, for context in the report.
    pub events: Vec<Event>,
}

impl Report {
    pub(crate) fn passed(&self) -> bool {
        self.overlaps.is_empty() && self.unsettled.is_empty() && self.starts > 0
    }

    fn at(&self, us: i64) -> String {
        format!("+{:.3}s", seconds(us - self.origin_us))
    }

    /// A summary, and for each problem the events in the seconds before it.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        let settles = self
            .events
            .iter()
            .filter(|e| matches!(e.what, What::Settled { .. }))
            .count();
        let _ = writeln!(
            out,
            "{} singleton starts, {} heals, {} overlaps, {} heals without exactly one runner",
            self.starts,
            settles,
            self.overlaps.len(),
            self.unsettled.len()
        );
        if self.starts == 0 {
            let _ = writeln!(
                out,
                "no `singleton starting` record in node-1.log or node-2.log: nothing was checked"
            );
        }
        for &(start, end) in &self.overlaps {
            let _ = writeln!(
                out,
                "\nOVERLAP {} to {} ({:.3} s): both nodes ran the singleton{}; events before it:",
                self.at(start),
                self.at(end),
                seconds(end - start),
                self.after_thaw(start)
                    .map(|n| format!(", from when node {n} was continued after a pause"))
                    .unwrap_or_default()
            );
            self.context(&mut out, start - 15_000_000, end);
        }
        for (at, runners) in &self.unsettled {
            let _ = writeln!(
                out,
                "\nUNSETTLED {}: runners {runners:?} after healing; events before it:",
                self.at(*at)
            );
            self.context(&mut out, at - 30_000_000, *at);
        }
        out
    }

    /// The node continued at `at`, if one was: an overlap that starts then
    /// is a paused node running on past its lease.
    fn after_thaw(&self, at: i64) -> Option<u32> {
        self.events
            .iter()
            .find(|e| e.at_us == at && e.what == What::Thaw)
            .map(|e| e.node)
    }

    fn context(&self, out: &mut String, from: i64, to: i64) {
        for e in self
            .events
            .iter()
            .filter(|e| (from..=to).contains(&e.at_us))
        {
            let _ = writeln!(out, "  {} node {} {:?}", self.at(e.at_us), e.node, e.what);
        }
    }
}

#[allow(clippy::cast_precision_loss)] // run lengths are far below 2^52 µs
fn seconds(us: i64) -> f64 {
    us as f64 / 1e6
}

/// Check a failover run's directory: `events.jsonl`, `node-1.log` and
/// `node-2.log`.
pub(crate) fn check_dir(dir: &Path, app: &str) -> anyhow::Result<Report> {
    let events = events::read(&dir.join("events.jsonl"))?;
    let mut records = event_records(&events);
    for node in [1, 2] {
        let path = dir.join(format!("node-{node}.log"));
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        records.extend(log_records(node, app, &text));
    }
    let horizon = records
        .iter()
        .map(|r| r.at_us)
        .chain(events.iter().map(|e| e.at_us))
        .max()
        .unwrap_or(0);
    let starts = records.iter().filter(|r| r.mark == Mark::Start).count();
    let overlaps = overlaps(
        &running(&records, 1, horizon),
        &running(&records, 2, horizon),
    );
    let unsettled = events
        .iter()
        .filter_map(|e| match &e.what {
            What::Settled { runners } if runners.len() != 1 => Some((e.at_us, runners.clone())),
            _ => None,
        })
        .collect();
    let origin_us = events.first().map_or(0, |e| e.at_us);
    Ok(Report {
        overlaps,
        unsettled,
        starts,
        origin_us,
        events,
    })
}

/// `line` without ANSI colour sequences.
fn strip_ansi(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\u{1b}') {
        return line.into();
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // ESC [ parameters final-byte
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out.into()
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod check_tests;
