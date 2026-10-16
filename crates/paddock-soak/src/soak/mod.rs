//! Everything but `main`: Unix only.

pub(crate) mod chaos;
pub(crate) mod check;
pub(crate) mod cluster;
pub(crate) mod events;
pub(crate) mod failover;
pub(crate) mod node;
pub(crate) mod proxy;
pub(crate) mod rng;
pub(crate) mod time;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

/// Long-running fault injection for Paddock (scripts/soak.sh). Exit status:
/// 0 when the run passed, 1 when it found a problem or could not run.
#[derive(Debug, Parser)]
#[command(name = "paddock-soak")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// A witness and a pair behind UDP proxies, under random faults; fails
    /// on any moment when both nodes ran the singleton, or a heal after
    /// which other than one node runs it.
    Failover {
        #[command(flatten)]
        run: Run,
        /// Length of each episode of faults before everything is healed.
        #[arg(long, default_value_t = 60)]
        episode_s: u64,
        #[arg(long, default_value_t = 200)]
        heartbeat_ms: u64,
        #[arg(long, default_value_t = 1000)]
        node_timeout_ms: u64,
        #[arg(long, default_value_t = 2000)]
        lease_ms: u64,
        #[arg(long, default_value_t = 300)]
        margin_ms: u64,
        /// A serial line between the pair, a second heartbeat path that the
        /// run cuts too: `pty` (two ptys joined by the runner), or
        /// `DEV_A,DEV_B`, two devices joined by a null-modem cable (node 1
        /// is relayed to `DEV_A` through a pty, node 2 opens `DEV_B`).
        #[arg(long)]
        serial: Option<String>,
    },
    /// Three member nodes behind UDP proxies, their instances linking and
    /// monitoring each other at random, under link and node faults; fails
    /// when, after a heal, the nodes disagree on a link or monitor.
    Cluster {
        #[command(flatten)]
        run: Run,
        /// Length of each episode of faults before everything is healed.
        #[arg(long, default_value_t = 30)]
        episode_s: u64,
        #[arg(long, default_value_t = 200)]
        heartbeat_ms: u64,
        #[arg(long, default_value_t = 1000)]
        node_timeout_ms: u64,
    },
    /// The chaos example on one node under random failing commands.
    Chaos {
        #[command(flatten)]
        run: Run,
        /// Fail if the node's resident memory grows by more than this after
        /// the first tenth of the run.
        #[arg(long, default_value_t = 256)]
        rss_growth_mib: u64,
    },
    /// Check a failover run's directory again.
    Check { dir: PathBuf },
}

#[derive(Debug, Args)]
struct Run {
    /// The run's directory: seed, events, configs, node logs, result.
    #[arg(long)]
    dir: PathBuf,
    /// The seed for the fault schedule (default: from the clock).
    #[arg(long)]
    seed: Option<u64>,
    #[arg(long, default_value_t = 10.0)]
    minutes: f64,
    /// The repository root: examples and built guests are found under it.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// The paddock binary (default: `<root>/target/release/paddock`).
    #[arg(long)]
    paddock: Option<PathBuf>,
}

/// Directory, root, paddock binary and seed, with the defaults applied and
/// the seed written to `<dir>/seed`.
fn prepare(run: Run) -> anyhow::Result<(PathBuf, PathBuf, PathBuf, u64, f64)> {
    std::fs::create_dir_all(&run.dir)?;
    let dir = std::fs::canonicalize(&run.dir)?;
    let root = std::fs::canonicalize(&run.root)?;
    let paddock = match run.paddock {
        Some(p) => std::fs::canonicalize(p)?,
        None => root.join("target/release/paddock"),
    };
    let seed = run
        .seed
        .unwrap_or_else(|| time::now_us().unsigned_abs() % 1_000_000_000);
    std::fs::write(dir.join("seed"), format!("{seed}\n"))?;
    println!("{}: seed {seed}", dir.display());
    Ok((dir, root, paddock, seed, run.minutes))
}

async fn execute(command: Command) -> anyhow::Result<bool> {
    match command {
        Command::Failover {
            run,
            episode_s,
            heartbeat_ms,
            node_timeout_ms,
            lease_ms,
            margin_ms,
            serial,
        } => {
            let (dir, root, paddock, seed, minutes) = prepare(run)?;
            failover::run(&failover::Args {
                dir,
                root,
                paddock,
                seed,
                minutes,
                episode: Duration::from_secs(episode_s),
                timings: failover::Timings {
                    heartbeat_ms,
                    node_timeout_ms,
                    lease_ms,
                    margin_ms,
                },
                serial: serial
                    .as_deref()
                    .map(failover::SerialSpec::parse)
                    .transpose()?,
            })
            .await
        }
        Command::Cluster {
            run,
            episode_s,
            heartbeat_ms,
            node_timeout_ms,
        } => {
            let (dir, root, paddock, seed, minutes) = prepare(run)?;
            cluster::run(&cluster::Args {
                dir,
                root,
                paddock,
                seed,
                minutes,
                episode: Duration::from_secs(episode_s),
                heartbeat_ms,
                node_timeout_ms,
            })
            .await
        }
        Command::Chaos {
            run,
            rss_growth_mib,
        } => {
            let (dir, root, paddock, seed, minutes) = prepare(run)?;
            chaos::run(&chaos::Args {
                dir,
                root,
                paddock,
                seed,
                minutes,
                rss_growth_mib,
            })
            .await
        }
        Command::Check { dir } => {
            let report = check::check_dir(&dir, failover::APP)?;
            print!("{}", report.render());
            Ok(report.passed())
        }
    }
}

/// `run`, unless SIGINT or SIGTERM comes first: dropping it then kills
/// the nodes it started, so none outlive the run.
async fn interruptible(run: impl Future<Output = anyhow::Result<bool>>) -> anyhow::Result<bool> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        result = run => result,
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("interrupted")),
        _ = term.recv() => Err(anyhow::anyhow!("terminated")),
    }
}

pub(crate) fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(interruptible(execute(cli.command))) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
