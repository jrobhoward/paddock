//! Native-process apps (ROADMAP N2, spike S19): an app's program runs as a
//! process the node spawns through `paddock-runner`, an Erlang-style port,
//! with a pid, mailbox, links, monitors and supervision like an instance.
//!
//! The instance's task (`instance::native`) drives the process over its
//! stdin and stdout with `paddock_native::protocol`'s frames, one callback
//! at a time, answering the process functions it asks for during a
//! callback. The runner restricts itself before the program runs
//! (Landlock, seccomp); the node limits it from outside ([`port`]: a cgroup
//! or `RLIMIT_DATA`, the parent-death signal), kills it past a callback's
//! deadline or a missed ping, and maps its end to an exit reason.

pub(crate) mod cgroup;
pub(crate) mod convert;
pub(crate) mod port;

use std::path::PathBuf;

use paddock_bundle::manifest::NativeProgram;

/// A native app's program, written where the runner can read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCode {
    /// The program's file.
    pub path: PathBuf,
    /// A rustbridge plugin or an executable.
    pub kind: NativeProgram,
}
