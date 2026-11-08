//! A seccomp filter for a native app: it closes what Landlock leaves open
//! before Linux 6.12 (signals to other processes) and what it never covers
//! (ptrace, other processes' memory), and refuses new sockets: a native
//! app's sockets come from the node, connected (ROADMAP Z8e). `connect`
//! would move a passed socket elsewhere and `accept` take connections
//! the node does not count; `io_uring`'s operations would bypass this
//! filter (its socket operation makes sockets without `socket`), so it is
//! refused whole.
//!
//! Each listed system call fails with `EPERM` when its condition holds;
//! everything else is allowed.
//!
//! | Call | Refused when |
//! | --- | --- |
//! | `kill`, `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo` | the target is not this process (a negative or zero pid is a group); for the lease watchdog, nor its child |
//! | `tkill`, `pidfd_send_signal`, `pidfd_getfd` | always |
//! | `ptrace`, `process_vm_readv`, `process_vm_writev` | always |
//! | `socket`, `connect`, `accept`, `accept4` | always |
//! | `io_uring_setup`, `io_uring_enter`, `io_uring_register` | always |

use std::collections::BTreeMap;

use seccompiler::{
    SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    TargetArch,
};

/// The filter's rules for a process that may signal only the processes
/// `pids` (itself, and for the lease watchdog its child too).
///
/// # Errors
///
/// A seccompiler error (none for valid input).
pub fn rules(pids: &[u32]) -> Result<BTreeMap<i64, Vec<SeccompRule>>, seccompiler::Error> {
    // One rule whose conditions all hold: the target is none of `pids`.
    let other = || {
        pids.iter()
            .map(|pid| {
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    u64::from(*pid),
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .and_then(SeccompRule::new)
            .map(|r| vec![r])
    };
    let mut rules = BTreeMap::new();
    for call in [
        libc::SYS_kill,
        libc::SYS_tgkill,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
    ] {
        rules.insert(call, other()?);
    }
    for call in [
        libc::SYS_tkill,
        libc::SYS_pidfd_send_signal,
        libc::SYS_pidfd_getfd,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_socket,
        libc::SYS_connect,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        // No rules: the call always matches.
        rules.insert(call, Vec::new());
    }
    Ok(rules)
}

/// Install the filter on every thread of this process.
///
/// # Errors
///
/// An unsupported architecture, or the kernel refused the filter.
pub fn install() -> Result<(), String> {
    install_for(&[std::process::id()])
}

/// Install the filter, with signals allowed to `pids`, on every thread of
/// this process.
///
/// # Errors
///
/// As [`install`].
pub fn install_for(pids: &[u32]) -> Result<(), String> {
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?;
    let filter = SeccompFilter::new(
        rules(pids).map_err(|e| e.to_string())?,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM.unsigned_abs()),
        arch,
    )
    .map_err(|e| e.to_string())?;
    let program: seccompiler::BpfProgram = filter
        .try_into()
        .map_err(|e: seccompiler::BackendError| e.to_string())?;
    seccompiler::apply_filter_all_threads(&program).map_err(|e| e.to_string())
}
