//! Native-process apps (spike S19): an app's program runs as a
//! process the node spawns through `bulkhead-runner`, an Erlang-style port,
//! with a pid, mailbox, links, monitors and supervision like an instance.
//!
//! The instance's task (`instance::native`) drives the process over its
//! stdin and stdout with `bulkhead_native::protocol`'s frames, one callback
//! at a time, answering the process functions it asks for during a
//! callback. The runner restricts itself before the program runs
//! (Landlock and seccomp; on FreeBSD Capsicum); the node
//! limits it from outside ([`port`]: a cgroup or `RLIMIT_DATA`, the
//! parent-death signal), kills it past a callback's deadline or a missed
//! ping, and maps its end to an exit reason. Native apps run on Linux and
//! FreeBSD; what differs between them is in [`os`].

pub(crate) mod cgroup;
pub(crate) mod convert;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub mod helper;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) mod link;
pub(crate) mod memory;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) mod os;
pub(crate) mod port;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) mod sender;
#[cfg(target_os = "linux")]
pub(crate) mod syscalls;

use std::path::PathBuf;

use bulkhead_bundle::manifest::NativeProgram;

/// What a native instance runs: a program through `bulkhead-runner`, or an
/// in-process test app (cargo feature `test-apps`).
#[derive(Debug, Clone)]
pub(crate) enum Program {
    /// A program, run as a process.
    Process(NativeCode),
    /// An in-process test app ([`crate::in_process`]).
    #[cfg(feature = "test-apps")]
    InProcess(crate::in_process::InProcessApp),
}

impl Program {
    /// A rustbridge plugin or an executable; an in-process app behaves as
    /// an executable.
    pub(crate) fn kind(&self) -> NativeProgram {
        match self {
            Self::Process(code) => code.kind,
            #[cfg(feature = "test-apps")]
            Self::InProcess(_) => NativeProgram::Executable,
        }
    }
}

/// A native app's program, written where the runner can read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCode {
    /// The program's file.
    pub path: PathBuf,
    /// A rustbridge plugin or an executable.
    pub kind: NativeProgram,
}

/// What a descriptor passed to an app is, which on FreeBSD sets the rights
/// its copy keeps ([`os::for_app`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Passed {
    /// A published bucket's snapshot: read and mapped read-only.
    Snapshot,
    /// A connected stream.
    Stream,
    /// A sender channel's shared memory.
    Memfd,
    /// A sender channel's door (the app reads it).
    Door,
    /// A sender channel's bell (the app writes it).
    Bell,
}
