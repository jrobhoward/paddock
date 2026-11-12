//! The system calls rustix lacks, each behind a safe function: the only
//! module of the helper allowed `unsafe` (the crate denies it elsewhere).
//!
//! - Starting processes: `fork` and `clone3` (Linux), `pdfork` (FreeBSD),
//!   `execve` with a prepared argument and environment list, `_exit`.
//! - FreeBSD: `rctl` rules, a process descriptor's signal (`pdkill`), and
//!   a kqueue of process descriptors (`EVFILT_PROCDESC`).
//! - A signal handler that writes the signal's number to a pipe, so the
//!   helper's one `poll` sees signals too.
//! - `close_range`, for a child before it executes.
//!
//! A child after `fork` may only make system calls: the functions here
//! that a child calls ([`exec`], [`exit_now`], [`close_from`]) allocate
//! nothing.

#![allow(unsafe_code)]

#[cfg(target_os = "freebsd")]
use std::ffi::CStr;
use std::ffi::{CString, c_char};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

/// A program, its arguments and its environment, ready for `execve`
/// with nothing left to allocate.
pub struct Exec {
    path: CString,
    // The strings the pointers below point into; never changed after
    // `new`, so the pointers stay valid as long as `self`.
    _argv: Vec<CString>,
    _envp: Vec<CString>,
    argv_ptrs: Vec<*const c_char>,
    envp_ptrs: Vec<*const c_char>,
}

impl Exec {
    /// `path` run as `argv[0]` then `args`, with exactly `env`.
    ///
    /// # Errors
    ///
    /// A string holds a NUL.
    pub fn new(path: &str, args: &[String], env: &[(String, String)]) -> io::Result<Self> {
        let nul = |_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in an argument");
        let path = CString::new(path).map_err(nul)?;
        let mut argv = vec![path.clone()];
        for a in args {
            argv.push(CString::new(a.as_str()).map_err(nul)?);
        }
        let envp = env
            .iter()
            .map(|(k, v)| CString::new(format!("{k}={v}")).map_err(nul))
            .collect::<io::Result<Vec<_>>>()?;
        let argv_ptrs = argv
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        let envp_ptrs = envp
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        Ok(Self {
            path,
            _argv: argv,
            _envp: envp,
            argv_ptrs,
            envp_ptrs,
        })
    }
}

/// Replace this process with `e`'s program; returns only on failure.
pub fn exec(e: &Exec) -> io::Error {
    // SAFETY: `path` and both lists are NUL-terminated C strings and
    // null-terminated pointer lists owned by `e`, alive for the call.
    unsafe { libc::execve(e.path.as_ptr(), e.argv_ptrs.as_ptr(), e.envp_ptrs.as_ptr()) };
    io::Error::last_os_error()
}

/// End this process now, without running destructors or `atexit`
/// handlers: a child after `fork` that could not execute.
pub fn exit_now(code: i32) -> ! {
    // SAFETY: `_exit` takes no pointers and does not return.
    unsafe { libc::_exit(code) }
}

/// Close every descriptor from `first` up, in a child before it
/// executes. Errors are ignored: what is left is close-on-exec anyway.
pub fn close_from(first: u32) {
    // SAFETY: `close_range` takes no pointers.
    unsafe {
        libc::close_range(first, u32::MAX, 0);
    }
}

/// Which side of a fork this is.
pub enum Forked {
    /// The child.
    Child,
    /// The parent, with the child's pid and handle: a pidfd (Linux) or a
    /// process descriptor (FreeBSD).
    Parent { pid: u32, handle: OwnedFd },
}

/// `pdfork` the helper; the parent gets the child's process descriptor
/// (close-on-exec). The helper has one thread, so the child's copy of it
/// is whole.
///
/// # Errors
///
/// The fork failed.
#[cfg(target_os = "freebsd")]
pub fn fork() -> io::Result<Forked> {
    let mut pd: libc::c_int = -1;
    // SAFETY: `pd` is a valid pointer for the call; the helper is single
    // threaded, so the child is a consistent copy.
    let pid = unsafe { libc::pdfork(&raw mut pd, libc::PD_CLOEXEC) };
    match pid {
        p if p < 0 => Err(io::Error::last_os_error()),
        0 => Ok(Forked::Child),
        p => Ok(Forked::Parent {
            pid: u32::try_from(p).map_err(io::Error::other)?,
            // SAFETY: the kernel just made `pd` for this process.
            handle: unsafe { OwnedFd::from_raw_fd(pd) },
        }),
    }
}

/// `clone3`'s arguments (`struct clone_args`, Linux 5.7).
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

#[cfg(target_os = "linux")]
const CLONE_PIDFD: u64 = 0x1000;
#[cfg(target_os = "linux")]
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// The namespaces a new instance gets on Linux: mount, PID, network, IPC
/// and UTS (spike S36). No user namespace: the helper is root.
#[cfg(target_os = "linux")]
pub const NAMESPACES: u64 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUTS) as u64;

/// `clone3` like `fork`, with `namespaces` (some of [`NAMESPACES`]) and
/// straight into the cgroup `cgroup` (an open directory), with a pidfd
/// for the parent.
///
/// # Errors
///
/// The clone failed.
#[cfg(target_os = "linux")]
pub fn clone3(namespaces: u64, cgroup: Option<BorrowedFd<'_>>) -> io::Result<Forked> {
    let mut pidfd: RawFd = -1;
    let mut flags = CLONE_PIDFD | (namespaces & NAMESPACES);
    if cgroup.is_some() {
        flags |= CLONE_INTO_CGROUP;
    }
    let mut args = CloneArgs {
        flags,
        pidfd: (&raw mut pidfd) as u64,
        exit_signal: libc::SIGCHLD as u64,
        cgroup: cgroup.map_or(0, |fd| u64::try_from(fd.as_raw_fd()).unwrap_or(0)),
        ..CloneArgs::default()
    };
    // SAFETY: `args` is a valid `clone_args` of the size given, and
    // `pidfd` a valid int to write; with no stack given the child runs on
    // a copy of this one, as after `fork`, and the helper is single
    // threaded.
    let pid = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &raw mut args,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    match pid {
        p if p < 0 => Err(io::Error::last_os_error()),
        0 => Ok(Forked::Child),
        p => Ok(Forked::Parent {
            pid: u32::try_from(p).map_err(io::Error::other)?,
            // SAFETY: `CLONE_PIDFD` made this descriptor for the parent.
            handle: unsafe { OwnedFd::from_raw_fd(pidfd) },
        }),
    }
}

/// Send `signal` through a process descriptor.
///
/// # Errors
///
/// The kernel refused.
#[cfg(target_os = "freebsd")]
pub fn pdkill(pd: BorrowedFd<'_>, signal: i32) -> io::Result<()> {
    // SAFETY: `pdkill` takes no pointers.
    if unsafe { libc::pdkill(pd.as_raw_fd(), signal) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "freebsd")]
unsafe extern "C" {
    fn rctl_add_rule(
        inbufp: *const c_char,
        inbuflen: libc::size_t,
        outbufp: *mut c_char,
        outbuflen: libc::size_t,
    ) -> libc::c_int;
    fn rctl_remove_rule(
        inbufp: *const c_char,
        inbuflen: libc::size_t,
        outbufp: *mut c_char,
        outbuflen: libc::size_t,
    ) -> libc::c_int;
}

/// Add an `rctl` rule (`process:<pid>:memoryuse:deny=<bytes>`).
///
/// # Errors
///
/// The kernel refused it (racct off: `kern.racct.enable=1` in
/// `/boot/loader.conf`).
#[cfg(target_os = "freebsd")]
pub fn rctl_add(rule: &CStr) -> io::Result<()> {
    let len = rule.to_bytes_with_nul().len();
    // SAFETY: `rule` is a NUL-terminated string of `len` bytes; no output
    // buffer is given.
    if unsafe { rctl_add_rule(rule.as_ptr(), len, std::ptr::null_mut(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Remove the `rctl` rules matching `filter` (`process:<pid>`); errors
/// are ignored, as there may be none left.
#[cfg(target_os = "freebsd")]
pub fn rctl_remove(filter: &CStr) {
    let len = filter.to_bytes_with_nul().len();
    // SAFETY: as for `rctl_add`.
    unsafe { rctl_remove_rule(filter.as_ptr(), len, std::ptr::null_mut(), 0) };
}

/// A kqueue that reports the exit of process descriptors added to it,
/// with their wait status (FreeBSD).
#[cfg(target_os = "freebsd")]
pub struct ProcQueue(OwnedFd);

#[cfg(target_os = "freebsd")]
impl ProcQueue {
    /// A new, empty queue, close-on-exec.
    ///
    /// # Errors
    ///
    /// The kernel refused.
    pub fn new() -> io::Result<Self> {
        Ok(Self(rustix::event::kqueue::kqueue()?))
    }

    /// The queue's descriptor, which polls readable once an exit is
    /// waiting.
    pub fn fd(&self) -> BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.0.as_fd()
    }

    /// Report `pd`'s exit, tagged `tag`.
    ///
    /// # Errors
    ///
    /// The kernel refused.
    pub fn add(&self, pd: BorrowedFd<'_>, tag: u64) -> io::Result<()> {
        // SAFETY: a zeroed `kevent` is valid; every field used is set.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = usize::try_from(pd.as_raw_fd()).map_err(io::Error::other)?;
        ev.filter = libc::EVFILT_PROCDESC;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_EXIT;
        ev.udata = usize::try_from(tag).map_err(io::Error::other)? as *mut libc::c_void;
        // SAFETY: one valid change, no events out, no timeout pointer.
        let r = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                &raw const ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// The exits waiting now, as `(tag, wait status)`, without blocking.
    #[must_use]
    pub fn exits(&self) -> Vec<(u64, i32)> {
        // SAFETY: a zeroed `kevent` array is valid output space.
        let mut evs: [libc::kevent; 32] = unsafe { std::mem::zeroed() };
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `evs` has room for its length; `zero` is a valid
        // timeout, so the call does not block.
        let n = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                std::ptr::null(),
                0,
                evs.as_mut_ptr(),
                32,
                &raw const zero,
            )
        };
        let n = usize::try_from(n).unwrap_or(0);
        evs[..n]
            .iter()
            .map(|e| (e.udata as usize as u64, i32::try_from(e.data).unwrap_or(-1)))
            .collect()
    }
}

/// A wait status as `(exit code, signal)`.
#[cfg(target_os = "freebsd")]
#[must_use]
pub fn decode_status(status: i32) -> (Option<i32>, Option<i32>) {
    if libc::WIFEXITED(status) {
        (Some(libc::WEXITSTATUS(status)), None)
    } else if libc::WIFSIGNALED(status) {
        (None, Some(libc::WTERMSIG(status)))
    } else {
        (None, None)
    }
}

/// Where the signal handler writes; -1 until [`signal_pipe`].
static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: libc::c_int) {
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    let byte = u8::try_from(signal).unwrap_or(0);
    // SAFETY: `write` is async-signal-safe; `byte` lives for the call. A
    // full pipe drops the byte, and the helper reads all it finds.
    unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
}

/// A pipe whose read end gets a byte, the signal's number, for each of
/// `signals` this process receives. Once per process.
///
/// # Errors
///
/// The pipe or a handler could not be made.
pub fn signal_pipe(signals: &[i32]) -> io::Result<OwnedFd> {
    use rustix::pipe::{PipeFlags, pipe_with};

    let (read, write) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)?;
    SIGNAL_FD.store(write.as_raw_fd(), Ordering::Relaxed);
    // The write end lives as long as the process.
    std::mem::forget(write);
    for &signal in signals {
        // SAFETY: a zeroed `sigaction` is valid; the handler only writes
        // a byte; `SA_RESTART` keeps other calls from failing with EINTR.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&raw mut action.sa_mask);
            if libc::sigaction(signal, &raw const action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(read)
}

/// Put `signal` back to its default action, in a child before it
/// executes (the handler would be reset by `execve` anyway, but a child
/// that fails before it would run it).
pub fn default_signal(signal: i32) {
    // SAFETY: `signal` takes no pointers.
    unsafe { libc::signal(signal, libc::SIG_DFL) };
}

/// A close-on-exec copy of `fd` at 64 or above, as a number: in a child
/// before it places descriptors, so no source sits on a target.
///
/// # Errors
///
/// The kernel refused.
pub fn dup_high(fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: `fcntl` takes no pointers here.
    let r = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 64) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Make `target` a copy of `fd`, open across exec (`dup2`), or with
/// `cloexec` closed at exec.
///
/// # Errors
///
/// The kernel refused.
pub fn dup_to(fd: RawFd, target: RawFd, cloexec: bool) -> io::Result<()> {
    // SAFETY: `dup2` and `fcntl` take no pointers; `target` is replaced as
    // `dup2` does, in a child that owns every descriptor it has.
    unsafe {
        if libc::dup2(fd, target) < 0 {
            return Err(io::Error::last_os_error());
        }
        if cloexec && libc::fcntl(target, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Write `bytes` to the raw descriptor `fd`, once, ignoring the result:
/// a child reporting why it failed.
pub fn write_raw(fd: RawFd, bytes: &[u8]) {
    // SAFETY: `bytes` is valid for its length for the call.
    unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
}

/// Read one byte from the raw descriptor `fd`, blocking: a child waiting
/// for the helper's go-ahead. `false` at end of file or on an error.
#[cfg(target_os = "freebsd")]
pub fn read_byte(fd: RawFd) -> bool {
    let mut b = 0u8;
    loop {
        // SAFETY: `b` is valid for one byte for the call.
        let n = unsafe { libc::read(fd, (&raw mut b).cast(), 1) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return n == 1;
    }
}

/// Take ownership of descriptor `fd`, which this process inherited, once
/// it is checked to be open.
///
/// # Errors
///
/// It is not open.
pub fn adopt(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: `fcntl(F_GETFD)` takes no pointers; it fails on a closed
    // descriptor.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open, inherited, and owned by nothing else in this process:
    // `main` adopts it once, before anything else opens a descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Drop this process to `uid` and `gid`, with no supplementary groups
/// (FreeBSD, in a child after the fork).
///
/// # Errors
///
/// The kernel refused.
#[cfg(target_os = "freebsd")]
pub fn drop_to(uid: u32, gid: u32) -> io::Result<()> {
    // SAFETY: a null list of length 0, and calls that take no pointers.
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0
            || libc::setresgid(gid, gid, gid) != 0
            || libc::setresuid(uid, uid, uid) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
