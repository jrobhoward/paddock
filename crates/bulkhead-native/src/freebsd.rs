//! FreeBSD calls that rustix lacks or gets wrong, for the node, the runner
//! and apps alike.

use std::io;

/// Whether this process is in capability mode (the runner put it there,
/// ROADMAP FB2).
#[must_use]
pub fn in_capability_mode() -> bool {
    let mut mode: libc::c_uint = 0;
    // SAFETY: an out parameter on the stack.
    unsafe { libc::cap_getmode(&raw mut mode) == 0 && mode != 0 }
}

/// No core dump, and no ptrace or `/proc` memory from other processes of
/// this user: `procctl(PROC_TRACE_CTL)` on this process, Linux's "not
/// dumpable". With `across_exec` it lasts across `execve`
/// (`PROC_TRACE_CTL_DISABLE_EXEC`); otherwise an exec of a readable
/// program ends it. rustix 1.1's `set_dumpable_behavior` passes the value
/// where FreeBSD wants a pointer to it, and fails with `EFAULT`.
///
/// # Errors
///
/// The kernel refused.
pub fn set_not_traceable(across_exec: bool) -> io::Result<()> {
    let mut how: libc::c_int = if across_exec {
        libc::PROC_TRACE_CTL_DISABLE_EXEC
    } else {
        libc::PROC_TRACE_CTL_DISABLE
    };
    // SAFETY: `how` is a live int for the call, which only reads it; pid 0
    // with `P_PID` is this process.
    let r = unsafe { libc::procctl(libc::P_PID, 0, libc::PROC_TRACE_CTL, (&raw mut how).cast()) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Capsicum rights (ROADMAP FB2): what each descriptor an app holds may be
/// used for, shared by the runner (what the app inherits) and the node
/// (what it passes later, on a duplicate). Rights a descriptor lacks are
/// refused with `ENOTCAPABLE`.
pub mod rights {
    /// A channel's shared memory: the rings, read and written in place.
    pub const MEMFD: &[u64] = &[
        libc::CAP_READ,
        libc::CAP_WRITE,
        libc::CAP_MMAP_RW,
        libc::CAP_FSTAT,
    ];

    /// A channel's bell: the app writes a byte to wake the node.
    pub const BELL: &[u64] = &[
        libc::CAP_WRITE,
        libc::CAP_EVENT,
        libc::CAP_FSTAT,
        libc::CAP_FCNTL,
    ];

    /// A channel's door: the app reads the node's wake-ups. The descriptor
    /// socket keeps the same rights.
    pub const DOOR: &[u64] = &[
        libc::CAP_READ,
        libc::CAP_EVENT,
        libc::CAP_FSTAT,
        libc::CAP_FCNTL,
    ];

    /// A published bucket's snapshot: mapped read-only, its size read.
    pub const SNAPSHOT: &[u64] = &[libc::CAP_READ, libc::CAP_MMAP_R, libc::CAP_FSTAT];

    /// A connected stream: what a connection is used for, and nothing that
    /// moves it (`CONNECT`, `BIND`, `ACCEPT`). `IOCTL` is limited to
    /// [`STREAM_IOCTLS`].
    pub const STREAM: &[u64] = &[
        libc::CAP_IOCTL,
        libc::CAP_READ,
        libc::CAP_WRITE,
        libc::CAP_EVENT,
        libc::CAP_FSTAT,
        libc::CAP_FCNTL,
        libc::CAP_SHUTDOWN,
        libc::CAP_GETPEERNAME,
        libc::CAP_GETSOCKNAME,
        libc::CAP_GETSOCKOPT,
        libc::CAP_SETSOCKOPT,
    ];

    /// The `ioctl`s a stream allows: Rust's `set_nonblocking` is
    /// `FIONBIO`, and `FIONREAD` counts the bytes waiting.
    pub const STREAM_IOCTLS: &[libc::c_ulong] = &[libc::FIONBIO, libc::FIONREAD];
}

/// `list` as a rights set.
fn rights_set(list: &[u64]) -> libc::cap_rights_t {
    // SAFETY: a zeroed rights set is what `__cap_rights_init` fills; the
    // variadic calls end with the 0 the C macros add.
    unsafe {
        let mut r: libc::cap_rights_t = std::mem::zeroed();
        libc::__cap_rights_init(libc::CAP_RIGHTS_VERSION, &raw mut r, 0u64);
        for right in list {
            libc::__cap_rights_set(&raw mut r, *right, 0u64);
        }
        r
    }
}

/// Limit `fd`'s `ioctl`s to `cmds` (as [`rights::STREAM_IOCTLS`]); it
/// needs the `IOCTL` right to use them.
///
/// # Errors
///
/// The kernel refused: `EBADF` for a descriptor that is not open.
pub fn limit_ioctls(fd: std::os::fd::RawFd, cmds: &[libc::c_ulong]) -> io::Result<()> {
    // SAFETY: `cmds` is a live slice of its length for the call, which
    // copies it.
    if unsafe { libc::cap_ioctls_limit(fd, cmds.as_ptr(), cmds.len()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Limit `fd`'s rights to `list` (one of [`rights`]); rights only shrink.
///
/// # Errors
///
/// The kernel refused: `EBADF` for a descriptor that is not open.
pub fn limit_rights(fd: std::os::fd::RawFd, list: &[u64]) -> io::Result<()> {
    let r = rights_set(list);
    // SAFETY: a rights set on the stack; any descriptor number.
    if unsafe { libc::cap_rights_limit(fd, &raw const r) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
