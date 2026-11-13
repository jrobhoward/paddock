//! The seccomp filters for a native app: an allow-list of system calls
//! (ROADMAP PS4). The kernel's system calls are an app's widest way out of
//! its fence, so the app gets the ones a program on the SDK needs and no
//! others.
//!
//! Three filters are installed, and for each call the kernel takes the
//! strictest answer among them:
//!
//! 1. [`allowed`]: a call outside the list raises `SIGSYS` in the caller
//!    (`SECCOMP_RET_TRAP`). [`crate::app::serve`] handles it by recording
//!    the call's number in the channel and ending the process, so the
//!    instance exits `crashed: syscall <name>`; a process with no handler
//!    dies of the signal. A missing entry shows up as that exit, in a
//!    test, rather than as an odd `EPERM` somewhere inside a library.
//! 2. [`rules`]: calls that are on the list but refused in some or all
//!    uses fail with `EPERM`, and the app runs on. These are refusals an
//!    app may meet in ordinary code (a library that tries a socket), or
//!    calls whose arguments decide.
//! 3. `clone3` fails with `ENOSYS`: its flags are in memory, where a
//!    filter cannot read them, and libc then falls back to `clone`, whose
//!    flags filter 2 checks.
//!
//! What filter 2 refuses:
//!
//! | Call | Refused when |
//! | --- | --- |
//! | `kill`, `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo` | the target is not this process (a negative or zero pid is a group); for the lease watchdog, nor its child |
//! | `tkill`, `pidfd_send_signal`, `pidfd_getfd` | always |
//! | `ptrace`, `process_vm_readv`, `process_vm_writev` | always |
//! | `socket`, `connect`, `accept` | always: an app's sockets come from the node, connected (ROADMAP Z8e); `connect` would move a passed socket elsewhere and `accept` take connections the node does not count |
//! | `accept4` | unless its descriptor is a listening socket a driver app was passed (`--accept-fd`, ROADMAP DA1); for any other app, always |
//! | `io_uring_setup`, `io_uring_enter`, `io_uring_register` | always: its operations would bypass these filters (its socket operation makes sockets without `socket`) |
//! | `clone` | with a flag that makes a namespace: a user namespace gives an app capabilities over what it then makes, and the kernel code behind them |
//! | `socketpair` | for any family but `AF_UNIX` |
//! | `ioctl` | for any request but the descriptor ones (`FIONBIO`, `FIONREAD`, `FIOCLEX`, `FIONCLEX`) and the terminal check (`TCGETS`) |
//!
//! What is not on the list at all, among others: `bind` and `listen`,
//! `unshare`, `setns`, `mount`, `chroot`, `pivot_root`, `bpf`, `keyctl`,
//! `perf_event_open`, `userfaultfd`, `memfd_create`, `execveat`, the
//! credential calls, kernel modules, System V IPC and POSIX message
//! queues. Files are fenced by Landlock, not here: the file calls are
//! allowed, and `ftruncate` cannot resize the channel's memfd, which is
//! sealed.

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// The system calls a native app may make, on every architecture.
const ALLOWED: &[i64] = &[
    // Memory.
    libc::SYS_brk,
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_mprotect,
    libc::SYS_mremap,
    libc::SYS_madvise,
    libc::SYS_membarrier,
    // Processes and threads. An app may fork inside its fence (ROADMAP,
    // non-goals); Landlock decides what it may execute.
    libc::SYS_clone,
    libc::SYS_clone3,
    libc::SYS_execve,
    libc::SYS_exit,
    libc::SYS_exit_group,
    libc::SYS_wait4,
    libc::SYS_waitid,
    libc::SYS_set_tid_address,
    libc::SYS_set_robust_list,
    libc::SYS_rseq,
    libc::SYS_futex,
    libc::SYS_sched_yield,
    libc::SYS_sched_getaffinity,
    libc::SYS_sched_setaffinity,
    libc::SYS_getcpu,
    libc::SYS_getpid,
    libc::SYS_getppid,
    libc::SYS_gettid,
    libc::SYS_getuid,
    libc::SYS_geteuid,
    libc::SYS_getgid,
    libc::SYS_getegid,
    libc::SYS_prctl,
    libc::SYS_prlimit64,
    libc::SYS_getrusage,
    libc::SYS_uname,
    libc::SYS_sysinfo,
    libc::SYS_getrandom,
    libc::SYS_restart_syscall,
    // Signals. Filter 2 limits whom the sending calls reach.
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sigaltstack,
    libc::SYS_kill,
    libc::SYS_tgkill,
    libc::SYS_tkill,
    libc::SYS_rt_sigqueueinfo,
    libc::SYS_rt_tgsigqueueinfo,
    libc::SYS_pidfd_send_signal,
    libc::SYS_pidfd_getfd,
    // Time.
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_nanosleep,
    libc::SYS_gettimeofday,
    libc::SYS_timerfd_create,
    libc::SYS_timerfd_settime,
    libc::SYS_timerfd_gettime,
    // Descriptors.
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_readv,
    libc::SYS_writev,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_preadv,
    libc::SYS_pwritev,
    libc::SYS_lseek,
    libc::SYS_close,
    libc::SYS_close_range,
    libc::SYS_dup,
    libc::SYS_dup3,
    libc::SYS_fcntl,
    libc::SYS_ioctl,
    libc::SYS_pipe2,
    libc::SYS_eventfd2,
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_pwait,
    libc::SYS_epoll_pwait2,
    libc::SYS_ppoll,
    libc::SYS_pselect6,
    // Files. Landlock decides which.
    libc::SYS_openat,
    libc::SYS_openat2,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_statx,
    libc::SYS_statfs,
    libc::SYS_fstatfs,
    libc::SYS_faccessat,
    libc::SYS_faccessat2,
    libc::SYS_readlinkat,
    libc::SYS_getdents64,
    libc::SYS_getcwd,
    libc::SYS_chdir,
    libc::SYS_fchdir,
    libc::SYS_umask,
    libc::SYS_mkdirat,
    libc::SYS_unlinkat,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_symlinkat,
    libc::SYS_linkat,
    libc::SYS_truncate,
    libc::SYS_ftruncate,
    libc::SYS_fallocate,
    libc::SYS_fadvise64,
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    libc::SYS_flock,
    libc::SYS_utimensat,
    libc::SYS_fchmod,
    libc::SYS_fchmodat,
    // With `copy_file_range`, what Rust's `io::copy` tries between two
    // descriptors.
    libc::SYS_copy_file_range,
    libc::SYS_sendfile,
    libc::SYS_splice,
    // Sockets the node passed. Filter 2 refuses making or moving one.
    libc::SYS_recvfrom,
    libc::SYS_sendto,
    libc::SYS_recvmsg,
    libc::SYS_sendmsg,
    libc::SYS_recvmmsg,
    libc::SYS_sendmmsg,
    libc::SYS_shutdown,
    libc::SYS_getsockopt,
    libc::SYS_setsockopt,
    libc::SYS_getsockname,
    libc::SYS_getpeername,
    libc::SYS_socketpair,
    libc::SYS_socket,
    libc::SYS_connect,
    libc::SYS_accept,
    libc::SYS_accept4,
    // Refused by filter 2 with `EPERM`, as before the allow-list.
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// The calls only x86-64 has: the forms `openat` and its like replaced,
/// which its libc still makes.
#[cfg(target_arch = "x86_64")]
const ALLOWED_ARCH: &[i64] = &[
    libc::SYS_arch_prctl,
    libc::SYS_open,
    libc::SYS_stat,
    libc::SYS_lstat,
    libc::SYS_access,
    libc::SYS_readlink,
    libc::SYS_getdents,
    libc::SYS_mkdir,
    libc::SYS_rmdir,
    libc::SYS_unlink,
    libc::SYS_rename,
    libc::SYS_symlink,
    libc::SYS_link,
    libc::SYS_chmod,
    libc::SYS_pipe,
    libc::SYS_dup2,
    libc::SYS_poll,
    libc::SYS_select,
    libc::SYS_epoll_wait,
    libc::SYS_fork,
    libc::SYS_vfork,
    libc::SYS_time,
    libc::SYS_getrlimit,
];

#[cfg(not(target_arch = "x86_64"))]
const ALLOWED_ARCH: &[i64] = &[];

/// The `clone` flags that make a namespace.
const NAMESPACE_FLAGS: [libc::c_int; 8] = [
    libc::CLONE_NEWNS,
    libc::CLONE_NEWCGROUP,
    libc::CLONE_NEWUTS,
    libc::CLONE_NEWIPC,
    libc::CLONE_NEWUSER,
    libc::CLONE_NEWPID,
    libc::CLONE_NEWNET,
    libc::CLONE_NEWTIME,
];

/// The `ioctl` requests an app may make: non-blocking mode, bytes
/// waiting, close-on-exec, and the terminal check behind `isatty`.
const IOCTLS: [libc::Ioctl; 5] = [
    libc::FIONBIO,
    libc::FIONREAD,
    libc::FIOCLEX,
    libc::FIONCLEX,
    libc::TCGETS,
];

/// The system calls a native app may make (filter 1): any other raises
/// `SIGSYS`.
#[must_use]
pub fn allowed() -> Vec<i64> {
    ALLOWED.iter().chain(ALLOWED_ARCH).copied().collect()
}

fn condition(
    arg: u8,
    op: SeccompCmpOp,
    value: u64,
) -> Result<SeccompCondition, seccompiler::Error> {
    // A double word: pids, flags and requests are 32 bits wide, and the
    // upper half of the register is not the kernel's to look at.
    Ok(SeccompCondition::new(
        arg,
        SeccompCmpArgLen::Dword,
        op,
        value,
    )?)
}

/// As an unsigned 32-bit value, the way the filter compares it.
fn dword(v: impl Into<i64>) -> u64 {
    // Truncation is the point: an `ioctl` request with its top bit set is
    // negative as a C `int` on some targets.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let low = v.into() as u32;
    u64::from(low)
}

/// The calls filter 2 refuses with `EPERM`, with the conditions under
/// which it does (none: always), for a process that may signal only the
/// processes `pids` (itself, and for the lease watchdog its child too).
///
/// # Errors
///
/// A seccompiler error (none for valid input).
pub fn rules(
    pids: &[u32],
    accept_fds: &[i32],
) -> Result<BTreeMap<i64, Vec<SeccompRule>>, seccompiler::Error> {
    let other = || {
        pids.iter()
            .map(|pid| condition(0, SeccompCmpOp::Ne, u64::from(*pid)))
            .collect::<Result<Vec<_>, _>>()
            .and_then(|c| Ok(SeccompRule::new(c)?))
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
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        rules.insert(call, Vec::new());
    }
    // `accept4` only on the listening sockets a driver app was passed
    // (ROADMAP DA1): refused unless its descriptor is one of them; with
    // none, always.
    let accept4 = if accept_fds.is_empty() {
        Vec::new()
    } else {
        let not_passed = accept_fds
            .iter()
            .map(|fd| condition(0, SeccompCmpOp::Ne, u64::from(fd.unsigned_abs())))
            .collect::<Result<Vec<_>, _>>()?;
        vec![SeccompRule::new(not_passed)?]
    };
    rules.insert(libc::SYS_accept4, accept4);
    // One rule per namespace flag: a rule's conditions are joined by
    // "and", the rules of a call by "or".
    let namespaces = NAMESPACE_FLAGS
        .iter()
        .map(|flag| {
            let flag = dword(*flag);
            SeccompRule::new(vec![condition(0, SeccompCmpOp::MaskedEq(flag), flag)?])
                .map_err(seccompiler::Error::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    rules.insert(libc::SYS_clone, namespaces);
    rules.insert(
        libc::SYS_socketpair,
        vec![SeccompRule::new(vec![condition(
            0,
            SeccompCmpOp::Ne,
            dword(libc::AF_UNIX),
        )?])?],
    );
    let unlisted_request = IOCTLS
        .iter()
        .map(|request| {
            // `Ioctl` is `c_ulong` with glibc and `c_int` with musl.
            #[allow(clippy::useless_conversion, clippy::cast_possible_wrap)]
            let request = *request as i64;
            condition(1, SeccompCmpOp::Ne, dword(request))
        })
        .collect::<Result<Vec<_>, _>>()?;
    rules.insert(libc::SYS_ioctl, vec![SeccompRule::new(unlisted_request)?]);
    Ok(rules)
}

/// The three filters, in the order they are installed (the module's
/// docs number them by what they do).
fn programs(pids: &[u32], accept_fds: &[i32]) -> Result<Vec<BpfProgram>, String> {
    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?;
    let compile = |rules, mismatch, matched| -> Result<BpfProgram, String> {
        SeccompFilter::new(rules, mismatch, matched, arch)
            .map_err(|e| e.to_string())?
            .try_into()
            .map_err(|e: seccompiler::BackendError| e.to_string())
    };
    let eperm = SeccompAction::Errno(libc::EPERM.unsigned_abs());
    let enosys = SeccompAction::Errno(libc::ENOSYS.unsigned_abs());
    let listed = allowed()
        .into_iter()
        .map(|call| (call, Vec::new()))
        .collect();
    let clone3 = BTreeMap::from([(libc::SYS_clone3, Vec::new())]);
    // The allow-list last: `seccomp` itself is not on it, so nothing can
    // be installed after it.
    Ok(vec![
        compile(clone3, SeccompAction::Allow, enosys)?,
        compile(
            rules(pids, accept_fds).map_err(|e| e.to_string())?,
            SeccompAction::Allow,
            eperm,
        )?,
        compile(listed, SeccompAction::Trap, SeccompAction::Allow)?,
    ])
}

/// Install the filters on every thread of this process, with signals
/// allowed to itself only. They stay in force across `execve` and in
/// anything the process starts.
///
/// # Errors
///
/// A filter could not be built or installed.
pub fn install(accept_fds: &[i32]) -> Result<(), String> {
    install_for(&[std::process::id()], accept_fds)
}

/// [`install`], with signals allowed to the processes `pids` (the lease
/// watchdog: itself and its child).
///
/// # Errors
///
/// A filter could not be built or installed.
pub fn install_for(pids: &[u32], accept_fds: &[i32]) -> Result<(), String> {
    for program in programs(pids, accept_fds)? {
        seccompiler::apply_filter_all_threads(&program).map_err(|e| e.to_string())?;
    }
    Ok(())
}
