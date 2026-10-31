//! A seccomp filter for a native app: it closes what Landlock leaves open
//! before Linux 6.12 (signals to other processes) and what it never covers
//! (ptrace, other processes' memory), and refuses new sockets, since a
//! native app has no network.
//!
//! Each listed system call fails with `EPERM` when its condition holds;
//! everything else is allowed.
//!
//! | Call | Refused when |
//! | --- | --- |
//! | `kill`, `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo` | the target is not this process (a negative or zero pid is a group) |
//! | `tkill`, `pidfd_send_signal`, `pidfd_getfd` | always |
//! | `ptrace`, `process_vm_readv`, `process_vm_writev` | always |
//! | `socket` | always |

use std::collections::BTreeMap;

use seccompiler::{
    SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    TargetArch,
};

/// The filter's rules for a process whose pid is `pid`.
///
/// # Errors
///
/// A seccompiler error (none for valid input).
pub fn rules(pid: u32) -> Result<BTreeMap<i64, Vec<SeccompRule>>, seccompiler::Error> {
    let other = || {
        SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, u64::from(pid))
            .and_then(|c| SeccompRule::new(vec![c]))
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
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?;
    let filter = SeccompFilter::new(
        rules(std::process::id()).map_err(|e| e.to_string())?,
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
