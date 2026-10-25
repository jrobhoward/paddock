//! Resident memory of a run's node processes. Every 10 s the sampler finds
//! this process's children that run `paddock node --config .../node-<n>.toml`
//! and reads each one's `VmRSS` (Linux; elsewhere nothing is sampled). A
//! node killed and started again is a new process, so growth is judged per
//! process: from its first sample after a minute's warm-up to its last.
//!
//! The run's directory gets `memory.csv` (seconds since the run started,
//! node, pid, KiB) as samples are taken, and `memory.txt` at the end: the
//! largest growth on its first line, for `scripts/soak.sh`'s summary, then
//! one line per process.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How often the nodes are sampled.
const EVERY: Duration = Duration::from_secs(10);

/// How long a process runs before its first sample counts as its start:
/// apps deploy and caches fill in the first seconds.
pub(crate) const WARM_UP_S: u64 = 60;

/// One reading of one node process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sample {
    /// Seconds since the sampler started.
    pub at_s: u64,
    pub node: u32,
    pub pid: u32,
    pub rss_kib: u64,
}

/// One node process's samples, summed up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lifetime {
    pub node: u32,
    pub pid: u32,
    /// Seconds between its first and last samples.
    pub span_s: u64,
    /// The first sample after the warm-up; `None` for a process that did
    /// not live that long.
    pub warm_kib: Option<u64>,
    pub peak_kib: u64,
    pub last_kib: u64,
}

impl Lifetime {
    /// KiB gained from the warm sample to the last.
    pub(crate) fn growth_kib(&self) -> Option<i64> {
        let warm = i64::try_from(self.warm_kib?).ok()?;
        Some(i64::try_from(self.last_kib).ok()? - warm)
    }
}

/// Group samples by process, in the order each was first seen.
pub(crate) fn lifetimes(samples: &[Sample], warm_up_s: u64) -> Vec<Lifetime> {
    let mut by_pid: BTreeMap<u32, (usize, Vec<Sample>)> = BTreeMap::new();
    for (i, s) in samples.iter().enumerate() {
        by_pid
            .entry(s.pid)
            .or_insert_with(|| (i, Vec::new()))
            .1
            .push(*s);
    }
    let mut out: Vec<(usize, Lifetime)> = by_pid
        .into_values()
        .filter_map(|(first_seen, group)| {
            let first = group.first()?;
            let last = group.last()?;
            let warm = group
                .iter()
                .find(|s| s.at_s - first.at_s >= warm_up_s)
                .map(|s| s.rss_kib);
            Some((
                first_seen,
                Lifetime {
                    node: first.node,
                    pid: first.pid,
                    span_s: last.at_s - first.at_s,
                    warm_kib: warm,
                    peak_kib: group.iter().map(|s| s.rss_kib).max().unwrap_or(0),
                    last_kib: last.rss_kib,
                },
            ))
        })
        .collect();
    out.sort_by_key(|(first_seen, _)| *first_seen);
    out.into_iter().map(|(_, l)| l).collect()
}

fn mib(kib: u64) -> String {
    format!("{:.1}", kib as f64 / 1024.0)
}

/// `memory.txt`: the headline, then a line per process.
pub(crate) fn render(lifetimes: &[Lifetime]) -> String {
    let mut text = match lifetimes
        .iter()
        .filter_map(|l| Some((l.growth_kib()?, l)))
        .max_by_key(|(growth, _)| *growth)
    {
        Some((growth, l)) => format!(
            "rss growth {:+.1} MiB at most (node {}, {} to {} MiB over {} min)\n",
            growth as f64 / 1024.0,
            l.node,
            mib(l.warm_kib.unwrap_or(0)),
            mib(l.last_kib),
            l.span_s / 60
        ),
        None if lifetimes.is_empty() => "rss not sampled\n".to_string(),
        None => format!("rss no process lived past the {WARM_UP_S} s warm-up\n"),
    };
    for l in lifetimes {
        let warm = l.warm_kib.map_or_else(|| "-".to_string(), mib);
        let _ = writeln!(
            text,
            "node {} pid {}: {} s sampled; MiB after warm-up {warm}, peak {}, last {}",
            l.node,
            l.pid,
            l.span_s,
            mib(l.peak_kib),
            mib(l.last_kib)
        );
    }
    text
}

/// The node number in a `paddock node --config .../node-<n>.toml` command
/// line (arguments separated by NULs, as `/proc/<pid>/cmdline` has them).
pub(crate) fn node_of(cmdline: &[u8]) -> Option<u32> {
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    if args.get(1) != Some(&b"node".as_slice()) {
        return None;
    }
    let at = args.iter().position(|a| *a == b"--config")?;
    let config = std::str::from_utf8(args.get(at + 1)?).ok()?;
    let name = Path::new(config).file_name()?.to_str()?;
    name.strip_prefix("node-")?
        .strip_suffix(".toml")?
        .parse()
        .ok()
}

/// A process's resident memory in KiB, from `/proc` (Linux).
pub(crate) fn rss_kib(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// This process's children running `paddock node`, as (node, pid).
fn nodes() -> Vec<(u32, u32)> {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| parent(*pid) == Some(me))
        .filter_map(|pid| {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
            Some((node_of(&cmdline)?, pid))
        })
        .collect()
}

/// The parent pid from `/proc/<pid>/stat` (after the parenthesized name,
/// which may hold spaces: state, then ppid).
fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?.1.split(' ').nth(1)?.parse().ok()
}

/// Samples the nodes until finished.
#[derive(Debug)]
pub(crate) struct Sampler {
    dir: PathBuf,
    samples: Arc<Mutex<Vec<Sample>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Sampler {
    /// Start sampling into `dir/memory.csv`.
    pub(crate) fn start(dir: &Path) -> Self {
        let samples = Arc::new(Mutex::new(Vec::new()));
        let kept = samples.clone();
        let csv = dir.join("memory.csv");
        let task = tokio::spawn(async move {
            let begin = Instant::now();
            let mut tick = tokio::time::interval(EVERY);
            loop {
                tick.tick().await;
                let at_s = begin.elapsed().as_secs();
                let taken: Vec<Sample> = nodes()
                    .into_iter()
                    .filter_map(|(node, pid)| {
                        Some(Sample {
                            at_s,
                            node,
                            pid,
                            rss_kib: rss_kib(pid)?,
                        })
                    })
                    .collect();
                if taken.is_empty() {
                    continue;
                }
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&csv)
                {
                    for s in &taken {
                        let _ = writeln!(f, "{},{},{},{}", s.at_s, s.node, s.pid, s.rss_kib);
                    }
                }
                kept.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend(taken);
            }
        });
        Self {
            dir: dir.to_path_buf(),
            samples,
            task,
        }
    }

    /// Stop, write `memory.txt` and print it.
    pub(crate) fn finish(self) -> std::io::Result<()> {
        self.task.abort();
        let samples = self
            .samples
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let text = render(&lifetimes(&samples, WARM_UP_S));
        std::fs::write(self.dir.join("memory.txt"), &text)?;
        print!("{text}");
        Ok(())
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod memory_tests;
