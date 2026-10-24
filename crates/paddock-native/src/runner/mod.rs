//! `paddock-runner`: the launcher a node runs every native app through.
//!
//! ```text
//! paddock-runner (--library PATH | --executable PATH) [--read DIR]... [--landlock required|best-effort]
//! ```
//!
//! In order, before any code from the bundle runs:
//!
//! 1. Not dumpable: no core dump (a `core_pattern` that pipes to a helper
//!    ignores `RLIMIT_CORE` and holds the dying process for about a
//!    second, spike S19), and no ptrace from other processes of the
//!    node's user.
//! 2. Landlock ([`sandbox::restrict`]): read the system libraries, the
//!    program's directory and each `--read` directory; execute only the
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
//! program, which serves itself through [`crate::app`]. Both restrictions
//! stay in force across the exec.

pub mod library;
pub mod sandbox;
pub mod seccomp;

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
    /// Whether to refuse to run without Landlock.
    pub landlock_required: bool,
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
        let mut landlock_required = true;
        while let Some(flag) = it.next() {
            let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--library" => program = Some(Program::Library(value()?.into())),
                "--executable" => program = Some(Program::Executable(value()?.into())),
                "--read" => read.push(value()?.into()),
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
            landlock_required,
        })
    }
}

/// The runner's `main`.
#[must_use]
pub fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("paddock-runner: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        eprintln!("paddock-runner: not dumpable: {e}");
        return ExitCode::from(2);
    }
    if let Err(e) = sandbox::restrict(&args) {
        eprintln!("paddock-runner: landlock: {e}");
        return ExitCode::from(2);
    }
    if let Err(e) = seccomp::install() {
        eprintln!("paddock-runner: seccomp: {e}");
        return ExitCode::from(2);
    }
    match args.program {
        Program::Library(path) => library::serve(path),
        Program::Executable(path) => {
            let err = std::process::Command::new(&path).env_clear().exec();
            eprintln!("paddock-runner: {}: {err}", path.display());
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod runner_tests;
