//! The lease watchdog ([`crate::lease`]): with [`crate::lease::FLAG`],
//! the runner forks before it restricts itself. The child goes on to be
//! the app; the parent watches the child and the lease pipe, kills the
//! child once the latest deadline has passed, and exits as the child did,
//! so the node sees one process with the app's exit.
//!
//! On Linux the parent restricts itself too: Landlock with no access at
//! all, and the app's seccomp filter with signals allowed to the child as
//! well as itself (FreeBSD's fence is ROADMAP FB2). It keeps only the
//! lease pipe and stderr open; the child's death signal is `SIGKILL`, so
//! killing the runner, as the node does, kills the app. It learns of the
//! child's exit from a pidfd on Linux and a kqueue's `NOTE_EXIT` on
//! FreeBSD, either polled beside the lease pipe.

use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};

#[cfg(target_os = "linux")]
use landlock::{ABI, Access, AccessFs, AccessNet, Ruleset, RulesetAttr};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, Signal};

use crate::lease::{KILLED_EXIT, LEASE_FD, NO_DEADLINE, last_deadline, now_ns};

/// Fork: return in the child, which becomes the app; never return in the
/// parent, which watches it.
///
/// # Errors
///
/// The fork failed, or the child could not tie its life to the parent's;
/// the runner then exits.
pub fn fork() -> Result<(), String> {
    let parent = rustix::process::getpid();
    // SAFETY: the runner has one thread here (it forks before it starts
    // any, as Landlock needs too), so the child's copy of the process is
    // consistent; both sides only make system calls and run plain Rust
    // afterwards.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if child == 0 {
        rustix::process::set_parent_process_death_signal(Some(Signal::KILL))
            .map_err(|e| format!("death signal: {e}"))?;
        // The parent may have died before the death signal was set.
        if rustix::process::getppid() != Some(parent) {
            return Err("the watchdog exited before the app started".into());
        }
        // SAFETY: the node put the lease pipe at this descriptor; nothing
        // else in the child owns it, and the app must not hold it.
        drop(unsafe { OwnedFd::from_raw_fd(LEASE_FD) });
        return Ok(());
    }
    let code = Pid::from_raw(child)
        .ok_or_else(|| "fork returned no pid".to_string())
        .and_then(watch);
    match code {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("bulkhead-runner: watchdog: {e}");
            // The app must not outlive a watchdog that cannot watch it.
            // SAFETY: `kill` takes no pointers.
            unsafe { libc::kill(child, libc::SIGKILL) };
            std::process::exit(2)
        }
    }
}

/// Watch `child` until it exits; its exit status for the runner's.
fn watch(child: Pid) -> Result<i32, String> {
    // Closing first: the exit watch takes the lowest free descriptor.
    close_all_but_lease();
    let exited = exit_watch(child)?;
    #[cfg(target_os = "linux")]
    restrict(child)?;
    // SAFETY: the node put the lease pipe's read end at this descriptor,
    // and this process owns it from here on.
    let lease = unsafe { OwnedFd::from_raw_fd(LEASE_FD) };
    rustix::fs::fcntl_setfl(&lease, rustix::fs::OFlags::NONBLOCK)
        .map_err(|e| format!("lease pipe: {e}"))?;
    let mut deadline = 0;
    let mut node_gone = false;
    let mut killed = false;
    loop {
        if !node_gone {
            match drain(lease.as_fd()) {
                Drained::Deadline(d) => deadline = d,
                Drained::Nothing => {}
                Drained::Closed => node_gone = true,
            }
        }
        let now = now_ns();
        if !killed && now >= deadline {
            eprintln!("bulkhead-runner: the lease ended; the app is killed");
            // SAFETY: `kill` takes no pointers.
            if unsafe { libc::kill(child.as_raw_nonzero().get(), libc::SIGKILL) } != 0 {
                return Err(format!("kill: {}", std::io::Error::last_os_error()));
            }
            killed = true;
        }
        let timeout = (!killed && deadline != NO_DEADLINE).then(|| {
            let left = deadline.saturating_sub(now);
            Timespec {
                tv_sec: i64::try_from(left / 1_000_000_000).unwrap_or(i64::MAX),
                tv_nsec: i64::try_from(left % 1_000_000_000).unwrap_or(0),
            }
        });
        let mut fds = vec![PollFd::new(&exited, PollFlags::IN)];
        if !node_gone {
            fds.push(PollFd::new(&lease, PollFlags::IN));
        }
        match poll(&mut fds, timeout.as_ref()) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(format!("poll: {e}")),
        }
        if fds[0].revents().contains(PollFlags::IN) {
            return Ok(exit_code(child, killed));
        }
    }
}

/// What reading the lease pipe found.
enum Drained {
    /// A new deadline.
    Deadline(u64),
    /// Nothing new.
    Nothing,
    /// The node closed its end.
    Closed,
}

fn drain(lease: BorrowedFd<'_>) -> Drained {
    let mut buf = [0u8; 512];
    let mut found = Drained::Nothing;
    loop {
        match rustix::io::read(lease, &mut buf) {
            Ok(0) => return Drained::Closed,
            Ok(n) => {
                if let Some(d) = last_deadline(&buf[..n]) {
                    found = Drained::Deadline(d);
                }
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return found,
        }
    }
}

/// Reap `child` and give its exit as this process's: its status, or its
/// signal raised again here; [`KILLED_EXIT`] when the lease killed it.
fn exit_code(child: Pid, killed: bool) -> i32 {
    let mut status = 0;
    // SAFETY: `status` is a valid pointer for the call's duration.
    let reaped = unsafe { libc::waitpid(child.as_raw_nonzero().get(), &raw mut status, 0) };
    if reaped < 0 {
        return 2;
    }
    if libc::WIFEXITED(status) {
        return libc::WEXITSTATUS(status);
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        if killed && signal == libc::SIGKILL {
            return i32::from(KILLED_EXIT);
        }
        // SAFETY: restoring a signal's default action and sending it to
        // this process take no pointers; the filter allows signals to
        // this process.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::kill(libc::getpid(), signal);
        }
        return 128 + signal;
    }
    2
}

/// Close every descriptor but stderr and the lease pipe: the app's ends
/// of the channel, its log pipe's stdout and stdin.
fn close_all_but_lease() {
    for fd in (0..LEASE_FD).filter(|fd| *fd != 2) {
        // SAFETY: closing a descriptor this process holds or not; nothing
        // in the watchdog uses these.
        unsafe { libc::close(fd) };
    }
    let first = u32::try_from(LEASE_FD + 1).unwrap_or(u32::MAX);
    // SAFETY: `close_range` takes no pointers.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::syscall(libc::SYS_close_range, first, u32::MAX, 0u32)
    };
    // SAFETY: as above.
    #[cfg(target_os = "freebsd")]
    unsafe {
        libc::close_range(first, u32::MAX, 0)
    };
}

/// A descriptor that polls readable once `child` has exited: its pidfd.
#[cfg(target_os = "linux")]
fn exit_watch(child: Pid) -> Result<OwnedFd, String> {
    rustix::process::pidfd_open(child, rustix::process::PidfdFlags::empty())
        .map_err(|e| format!("pidfd: {e}"))
}

/// A descriptor that polls readable once `child` has exited: a kqueue
/// watching it for `NOTE_EXIT`. The child cannot have been reaped yet
/// (only this process reaps it), so a child that already exited is still
/// found and reported at once.
#[cfg(target_os = "freebsd")]
fn exit_watch(child: Pid) -> Result<OwnedFd, String> {
    use rustix::event::kqueue::{Event, EventFilter, EventFlags, ProcessEvents, kevent, kqueue};

    let kq = kqueue().map_err(|e| format!("kqueue: {e}"))?;
    let watch = [Event::new(
        EventFilter::Proc {
            pid: child,
            flags: ProcessEvents::EXIT,
        },
        EventFlags::ADD | EventFlags::ONESHOT,
        std::ptr::null_mut(),
    )];
    let mut none: Vec<Event> = Vec::new();
    // SAFETY: the event's user data is null and never dereferenced.
    unsafe { kevent(&kq, &watch, &mut none, None) }.map_err(|e| format!("kevent: {e}"))?;
    Ok(kq)
}

/// Landlock with nothing allowed, then the seccomp filter with signals to
/// `child` as well. Not Landlock's signal scope: the child restricts
/// itself in a domain of its own, outside this one, which the scope would
/// keep this process from signalling.
#[cfg(target_os = "linux")]
fn restrict(child: Pid) -> Result<(), String> {
    let abi = ABI::V6;
    Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.handle_access(AccessNet::from_all(abi)))
        .and_then(Ruleset::create)
        .and_then(landlock::RulesetCreated::restrict_self)
        .map_err(|e| format!("landlock: {e}"))?;
    let me = u32::try_from(rustix::process::getpid().as_raw_nonzero().get()).unwrap_or(0);
    let child = u32::try_from(child.as_raw_nonzero().get()).unwrap_or(0);
    super::seccomp::install_for(&[me, child])
}
