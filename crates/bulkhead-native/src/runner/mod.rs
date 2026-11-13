//! `bulkhead-runner`: the launcher a node runs every native app through.
//!
//! ```text
//! bulkhead-runner (--library PATH | --executable PATH) [--read DIR]... [--write DIR]... [--landlock required|best-effort] [--lease] [--accept-fd N]...
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
//! On FreeBSD (ROADMAP FB2) step 1 is
//! `procctl(PROC_TRACE_CTL_DISABLE_EXEC)`, which lasts across the exec,
//! and steps 2 and 3 are Capsicum (`capsicum::exec`, FreeBSD only): the runner limits
//! each inherited descriptor's rights, opens the preopens as descriptors,
//! enters capability mode and executes the program through the run-time
//! linker. `--landlock best-effort` changes nothing there, and a
//! rustbridge plugin is refused (its loading would need paths). The jail
//! per instance comes with the helper (FB2's second half).

#[cfg(target_os = "freebsd")]
pub mod capsicum;
pub mod library;
#[cfg(target_os = "linux")]
pub mod sandbox;
#[cfg(target_os = "linux")]
pub mod seccomp;
pub mod watchdog;

#[cfg(not(target_os = "freebsd"))]
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
    /// Listening sockets a driver app was passed (ROADMAP DA1), by
    /// descriptor number: `accept4` is allowed on these alone.
    pub accept_fds: Vec<i32>,
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
        let mut accept_fds = Vec::new();
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
                "--accept-fd" => {
                    let v = value()?;
                    accept_fds.push(
                        v.parse()
                            .map_err(|_| format!("--accept-fd {v}: a descriptor number"))?,
                    );
                }
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
            accept_fds,
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
    if let Err(e) = seccomp::install(&args.accept_fds) {
        eprintln!("bulkhead-runner: seccomp: {e}");
        return ExitCode::from(2);
    }
    match &args.program {
        // A plugin is loaded by path at `init`, which capability mode
        // refuses (ROADMAP FB2).
        #[cfg(target_os = "freebsd")]
        Program::Library(_) => {
            eprintln!(
                "bulkhead-runner: a rustbridge plugin cannot run in capability mode \
                 on FreeBSD (it is loaded by path); build the app as an executable"
            );
            ExitCode::from(2)
        }
        #[cfg(not(target_os = "freebsd"))]
        Program::Library(path) => library::serve(path.clone()),
        Program::Executable(path) => {
            // The environment is the node's choice, already narrowed to
            // what the app may know.
            let env: Vec<(String, String)> = [
                crate::protocol::PRIV_ENV,
                crate::preopens::ENV,
                crate::channel::SPIN_ENV,
                crate::channel::SENDER_SPIN_ENV,
                crate::driver::LISTENERS_ENV,
            ]
            .into_iter()
            .filter_map(|var| std::env::var(var).ok().map(|v| (var.to_string(), v)))
            .collect();
            #[cfg(target_os = "freebsd")]
            return capsicum::exec(&args, path, env);
            #[cfg(not(target_os = "freebsd"))]
            {
                let mut cmd = std::process::Command::new(path);
                cmd.env_clear().envs(env);
                let err = cmd.exec();
                eprintln!("bulkhead-runner: {}: {err}", path.display());
                ExitCode::from(2)
            }
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
