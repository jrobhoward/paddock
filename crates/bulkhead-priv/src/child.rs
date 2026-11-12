//! What a new process does between the fork and its `execve`, for the
//! node and for each instance: prepared whole in the helper before the
//! fork ([`Plan`]), so the child makes system calls and nothing else.
//!
//! In order (spike S36, finding 6):
//! 1. signals back to their default actions, and a session of its own;
//! 2. the system's own steps ([`crate::platform::child_fences`]): on Linux
//!    private mounts, a host name and the minimal root; on FreeBSD the
//!    wait for the helper's `rctl` rules;
//! 3. descriptors moved to their numbers, `/dev/null` as stdin, every
//!    other descriptor closed;
//! 4. no core dump, then the drop to its user and group, with no
//!    supplementary groups;
//! 5. the parent-death signal (a credential change clears it, so after
//!    the drop) and `no_new_privs`;
//! 6. `execve` with exactly the environment given.
//!
//! A failing step writes its number and errno to an error pipe, which is
//! close-on-exec, and the child exits: the helper reads either that or the
//! pipe's end of file, which means the program runs.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use rustix::process::{Resource, Rlimit, Signal};

use crate::sys::{self, Exec};

/// The number the error pipe is moved to in the child, past every target.
const ERR_FD: RawFd = bulkhead_priv_wire::MAX_FD_TARGET as RawFd + 1;

/// The steps, by number, as the error pipe reports them.
pub const STEPS: [&str; 10] = [
    "signals",
    "session",
    "fences",
    "descriptors",
    "stdin",
    "core limit",
    "user and group",
    "parent-death signal",
    "no_new_privs",
    "exec",
];

/// A child's whole course after the fork.
pub struct Plan {
    /// What it executes.
    pub exec: Exec,
    /// Descriptors and the numbers they get: `(source, target)`.
    pub fds: Vec<(RawFd, RawFd)>,
    /// The user and group it runs as.
    pub uid: u32,
    /// Its group (no supplementary groups).
    pub gid: u32,
    /// The system's own steps.
    pub fences: crate::platform::ChildFences,
    /// The error pipe's write end.
    pub err: RawFd,
}

/// The signals the helper handles, which a child puts back to default.
pub const HANDLED: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// Report `step`'s failure on the error pipe `err` and exit.
fn fail(err: RawFd, step: u8, e: &io::Error) -> ! {
    let errno = e.raw_os_error().unwrap_or(0);
    let mut b = [0u8; 5];
    b[0] = step;
    b[1..].copy_from_slice(&errno.to_le_bytes());
    sys::write_raw(err, &b);
    sys::exit_now(127)
}

/// Run `plan` in the child; never returns.
pub fn run(plan: &Plan) -> ! {
    let mut err = plan.err;
    for s in HANDLED {
        sys::default_signal(s);
    }
    if let Err(e) = rustix::process::setsid() {
        fail(err, 1, &e.into());
    }
    if let Err(e) = crate::platform::child_fences(&plan.fences) {
        fail(err, 2, &e);
    }
    // Every source above every target first, the error pipe too, so no
    // placement overwrites a descriptor still to be placed.
    let mut high = [0 as RawFd; bulkhead_priv_wire::MAX_FDS];
    for (i, (src, _)) in plan.fds.iter().enumerate() {
        match sys::dup_high(*src) {
            Ok(fd) => high[i] = fd,
            Err(e) => fail(err, 3, &e),
        }
    }
    match sys::dup_high(err) {
        Ok(fd) => err = fd,
        Err(e) => fail(err, 3, &e),
    }
    for (i, (_, target)) in plan.fds.iter().enumerate() {
        if let Err(e) = sys::dup_to(high[i], *target, false) {
            fail(err, 3, &e);
        }
    }
    if let Err(e) = sys::dup_to(err, ERR_FD, true) {
        fail(err, 3, &e);
    }
    err = ERR_FD;
    if !plan.fds.iter().any(|(_, t)| *t == 0) {
        match rustix::fs::open(
            c"/dev/null",
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(null) => {
                if let Err(e) = sys::dup_to(null.as_raw_fd(), 0, false) {
                    fail(err, 4, &e);
                }
            }
            Err(e) => fail(err, 4, &e.into()),
        }
    }
    sys::close_from(u32::try_from(ERR_FD + 1).unwrap_or(u32::MAX));
    let none = Rlimit {
        current: Some(0),
        maximum: Some(0),
    };
    if let Err(e) = rustix::process::setrlimit(Resource::Core, none) {
        fail(err, 5, &e.into());
    }
    if let Err(e) = drop_to(plan.uid, plan.gid) {
        fail(err, 6, &e);
    }
    if let Err(e) = rustix::process::set_parent_process_death_signal(Some(Signal::KILL)) {
        fail(err, 7, &e.into());
    }
    if let Err(e) = no_new_privs() {
        fail(err, 8, &e);
    }
    fail(err, 9, &sys::exec(&plan.exec))
}

/// To `uid` and `gid`, with no supplementary groups: rustix's calls for
/// the one thread a forked child has (Linux).
#[cfg(target_os = "linux")]
fn drop_to(uid: u32, gid: u32) -> io::Result<()> {
    use rustix::process::{Gid, Uid};

    rustix::thread::set_thread_groups(&[])?;
    let g = Gid::from_raw(gid);
    rustix::thread::set_thread_res_gid(g, g, g)?;
    let u = Uid::from_raw(uid);
    rustix::thread::set_thread_res_uid(u, u, u)?;
    Ok(())
}

/// To `uid` and `gid`, with no supplementary groups (FreeBSD: rustix has
/// no such calls there).
#[cfg(target_os = "freebsd")]
fn drop_to(uid: u32, gid: u32) -> io::Result<()> {
    sys::drop_to(uid, gid)
}

#[cfg(target_os = "linux")]
fn no_new_privs() -> io::Result<()> {
    Ok(rustix::thread::set_no_new_privs(true)?)
}

#[cfg(target_os = "freebsd")]
fn no_new_privs() -> io::Result<()> {
    Ok(rustix::process::set_no_new_privs(None)?)
}

/// After the fork, in the helper: `Ok` once the child executed its
/// program (the pipe's end of file), or the step that failed and why.
///
/// # Errors
///
/// What the child reported.
pub fn outcome(read_end: OwnedFd) -> Result<(), String> {
    let mut b = [0u8; 5];
    let mut got = 0;
    while got < b.len() {
        match rustix::io::read(&read_end, &mut b[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(format!("error pipe: {e}")),
        }
    }
    if got == 0 {
        return Ok(());
    }
    let step = STEPS.get(usize::from(b[0])).copied().unwrap_or("?");
    let errno = i32::from_le_bytes([b[1], b[2], b[3], b[4]]);
    Err(format!("{step}: {}", io::Error::from_raw_os_error(errno)))
}

/// A close-on-exec pipe for a child's errors.
///
/// # Errors
///
/// The kernel refused.
pub fn error_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?)
}
