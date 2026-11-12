//! `bulkhead-runner`: the launcher a node runs every native app through.
//!
//! ```text
//! bulkhead-runner (--library PATH | --executable PATH) [--read DIR]... [--write DIR]... [--landlock required|best-effort] [--lease]
//! ```
//!
//! In order, before any code from the bundle runs:
//!
//! 0. With `--lease`, for an instance bound to a failover lease: fork a
//!    watchdog ([`watchdog`], [`crate::lease`]) that kills the app once
//!    the lease's last deadline from the node has passed, whether or not
//!    the node runs. The steps below run in the child.
//! 1. Not dumpable: no core dump (a `core_pattern` that pipes to a helper
//!    ignores `RLIMIT_CORE` and holds the dying process for about a
//!    second, spike S19), and no ptrace from other processes of the
//!    node's user.
//! 2. Landlock ([`sandbox::restrict`]): read the system libraries, the
//!    program's directory and each `--read` directory; read and write
//!    each `--write` directory (no executing from it); execute only the
//!    program; nothing else on the filesystem; no TCP. Set while the
//!    runner has one thread, since before Linux 7.1 Landlock covers only
//!    the calling thread and the threads it starts later. With
//!    `--landlock required` (the default) a kernel without Landlock is an
//!    error.
//! 3. seccomp ([`seccomp::install`]): signals only to this process, no
//!    ptrace or access to other processes' memory, no new sockets.
//!
//! Then `--library` loads a rustbridge plugin and serves it
//! ([`library`]), and `--executable` replaces the runner with the
//! program, which serves itself through [`crate::app`]. Landlock and
//! seccomp stay in force across the exec; not being dumpable does not
//! (`execve` resets it for a readable program), so [`crate::app::serve`]
//! sets it again.
//!
//! On FreeBSD, steps 2 and 3 have no equivalent yet: Capsicum and a jail
//! per instance are ROADMAP FB2. Until then the runner refuses to start
//! an app there unless the node passed `--landlock best-effort` (node
//! config `[native] landlock_optional`), which then runs it with no fence
//! but the node's uid and its limits. Step 1 is
//! `procctl(PROC_TRACE_CTL_DISABLE_EXEC)`, which lasts across the exec.

pub mod library;
#[cfg(target_os = "linux")]
pub mod sandbox;
#[cfg(target_os = "linux")]
pub mod seccomp;
pub mod watchdog;

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::ExitCode;

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Program {
    /// A rustbridge plugin, loaded into the runner.
    Library(PathBuf),
    /// An executable linking [`crate::app`], run in the runner's place.
    Executable(PathBuf),
}

impl Program {
    /// The program's file.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::Library(p) | Self::Executable(p) => p,
        }
    }
}

/// The runner's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// What to run.
    pub program: Program,
    /// Directories the program may read, beyond the system libraries and
    /// its own directory.
    pub read: Vec<PathBuf>,
    /// Directories the program may read and write (a read-write volume).
    pub write: Vec<PathBuf>,
    /// Whether to refuse to run without Landlock.
    pub landlock_required: bool,
    /// Whether a lease bounds the app ([`crate::lease`]): fork a watchdog.
    pub lease: bool,
}

impl Args {
    /// Parse the runner's arguments (without the program name).
    ///
    /// # Errors
    ///
    /// What is wrong with them.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut it = args.into_iter();
        let mut program = None;
        let mut read = Vec::new();
        let mut write = Vec::new();
        let mut landlock_required = true;
        let mut lease = false;
        while let Some(flag) = it.next() {
            if flag == crate::lease::FLAG {
                lease = true;
                continue;
            }
            let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--library" => program = Some(Program::Library(value()?.into())),
                "--executable" => program = Some(Program::Executable(value()?.into())),
                "--read" => read.push(value()?.into()),
                "--write" => write.push(value()?.into()),
                "--landlock" => {
                    landlock_required = match value()?.as_str() {
                        "required" => true,
                        "best-effort" => false,
                        other => {
                            return Err(format!("--landlock {other}: required or best-effort"));
                        }
                    }
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        let program = program.ok_or("--library or --executable is required")?;
        Ok(Self {
            program,
            read,
            write,
            landlock_required,
            lease,
        })
    }
}

/// The runner's `main`.
#[must_use]
pub fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("bulkhead-runner: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = not_dumpable() {
        eprintln!("bulkhead-runner: not dumpable: {e}");
        return ExitCode::from(2);
    }
    #[cfg(target_os = "freebsd")]
    if args.landlock_required {
        eprintln!(
            "bulkhead-runner: no sandbox on FreeBSD yet (ROADMAP FB2); \
             node config [native] landlock_optional = true runs apps without one"
        );
        return ExitCode::from(2);
    }
    if args.lease
        && let Err(e) = watchdog::fork()
    {
        eprintln!("bulkhead-runner: lease: {e}");
        return ExitCode::from(2);
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = sandbox::restrict(&args) {
        eprintln!("bulkhead-runner: landlock: {e}");
        return ExitCode::from(2);
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = seccomp::install() {
        eprintln!("bulkhead-runner: seccomp: {e}");
        return ExitCode::from(2);
    }
    match args.program {
        Program::Library(path) => library::serve(path),
        Program::Executable(path) => {
            // The environment is the node's choice, already narrowed to
            // what the app may know.
            let mut cmd = std::process::Command::new(&path);
            cmd.env_clear();
            for var in [
                crate::protocol::PRIV_ENV,
                crate::preopens::ENV,
                crate::channel::SPIN_ENV,
            ] {
                if let Some(value) = std::env::var_os(var) {
                    cmd.env(var, value);
                }
            }
            let err = cmd.exec();
            eprintln!("bulkhead-runner: {}: {err}", path.display());
            ExitCode::from(2)
        }
    }
}

/// No core dump, and no ptrace from other processes of the node's user.
#[cfg(target_os = "linux")]
fn not_dumpable() -> rustix::io::Result<()> {
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
}

/// As on Linux, and kept across the exec into the app.
#[cfg(target_os = "freebsd")]
fn not_dumpable() -> std::io::Result<()> {
    crate::freebsd::set_not_traceable(true)
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod runner_tests;
