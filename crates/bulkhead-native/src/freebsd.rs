//! FreeBSD calls that rustix lacks or gets wrong, for the node, the runner
//! and apps alike.

use std::io;

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
