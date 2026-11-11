//! Names for the system calls a native app's seccomp filter refuses, for
//! its exit reason (`crashed: syscall bind`; ROADMAP PS4).

/// The calls most likely to be met outside the allow-list
/// (`bulkhead_native::runner::seccomp`); any other is shown by number.
const NAMES: &[(libc::c_long, &str)] = &[
    (libc::SYS_bind, "bind"),
    (libc::SYS_listen, "listen"),
    (libc::SYS_unshare, "unshare"),
    (libc::SYS_setns, "setns"),
    (libc::SYS_mount, "mount"),
    (libc::SYS_umount2, "umount2"),
    (libc::SYS_chroot, "chroot"),
    (libc::SYS_pivot_root, "pivot_root"),
    (libc::SYS_bpf, "bpf"),
    (libc::SYS_keyctl, "keyctl"),
    (libc::SYS_add_key, "add_key"),
    (libc::SYS_request_key, "request_key"),
    (libc::SYS_perf_event_open, "perf_event_open"),
    (libc::SYS_userfaultfd, "userfaultfd"),
    (libc::SYS_memfd_create, "memfd_create"),
    (libc::SYS_execveat, "execveat"),
    (libc::SYS_seccomp, "seccomp"),
    (libc::SYS_setuid, "setuid"),
    (libc::SYS_setgid, "setgid"),
    (libc::SYS_setresuid, "setresuid"),
    (libc::SYS_setresgid, "setresgid"),
    (libc::SYS_setgroups, "setgroups"),
    (libc::SYS_capset, "capset"),
    (libc::SYS_capget, "capget"),
    (libc::SYS_setsid, "setsid"),
    (libc::SYS_setpgid, "setpgid"),
    (libc::SYS_setpriority, "setpriority"),
    (libc::SYS_sched_setscheduler, "sched_setscheduler"),
    (libc::SYS_sched_setattr, "sched_setattr"),
    (libc::SYS_setrlimit, "setrlimit"),
    (libc::SYS_personality, "personality"),
    (libc::SYS_init_module, "init_module"),
    (libc::SYS_finit_module, "finit_module"),
    (libc::SYS_delete_module, "delete_module"),
    (libc::SYS_kexec_load, "kexec_load"),
    (libc::SYS_reboot, "reboot"),
    (libc::SYS_swapon, "swapon"),
    (libc::SYS_mknodat, "mknodat"),
    (libc::SYS_fchown, "fchown"),
    (libc::SYS_fchownat, "fchownat"),
    (libc::SYS_setxattr, "setxattr"),
    (libc::SYS_getxattr, "getxattr"),
    (libc::SYS_name_to_handle_at, "name_to_handle_at"),
    (libc::SYS_open_by_handle_at, "open_by_handle_at"),
    (libc::SYS_inotify_init1, "inotify_init1"),
    (libc::SYS_fanotify_init, "fanotify_init"),
    (libc::SYS_signalfd4, "signalfd4"),
    (libc::SYS_pidfd_open, "pidfd_open"),
    (libc::SYS_mlock, "mlock"),
    (libc::SYS_mlockall, "mlockall"),
    (libc::SYS_mbind, "mbind"),
    (libc::SYS_set_mempolicy, "set_mempolicy"),
    (libc::SYS_shmget, "shmget"),
    (libc::SYS_shmat, "shmat"),
    (libc::SYS_msgget, "msgget"),
    (libc::SYS_semget, "semget"),
    (libc::SYS_mq_open, "mq_open"),
    (libc::SYS_vmsplice, "vmsplice"),
    (libc::SYS_tee, "tee"),
    (libc::SYS_io_setup, "io_setup"),
    (libc::SYS_timer_create, "timer_create"),
    (libc::SYS_setitimer, "setitimer"),
    (libc::SYS_sync, "sync"),
    (libc::SYS_syncfs, "syncfs"),
    (libc::SYS_msync, "msync"),
    (libc::SYS_mincore, "mincore"),
    (libc::SYS_pkey_alloc, "pkey_alloc"),
    (libc::SYS_landlock_create_ruleset, "landlock_create_ruleset"),
    (libc::SYS_process_madvise, "process_madvise"),
    (libc::SYS_memfd_secret, "memfd_secret"),
    (libc::SYS_syslog, "syslog"),
    (libc::SYS_acct, "acct"),
    (libc::SYS_quotactl, "quotactl"),
    (libc::SYS_times, "times"),
    (libc::SYS_getpgid, "getpgid"),
    (libc::SYS_getsid, "getsid"),
    (libc::SYS_getgroups, "getgroups"),
    (libc::SYS_getresuid, "getresuid"),
    (libc::SYS_getresgid, "getresgid"),
    (libc::SYS_getpriority, "getpriority"),
    (libc::SYS_sched_getscheduler, "sched_getscheduler"),
    (libc::SYS_sched_getparam, "sched_getparam"),
    (libc::SYS_get_robust_list, "get_robust_list"),
    (libc::SYS_kcmp, "kcmp"),
];

/// The exit reason of a process that died of `SIGSYS`, from the word its
/// handler left in the channel (`refused`: 1 plus the call's number, or 0
/// when it left none: an app that does not use
/// `bulkhead_native::app::serve`, or a call before it).
pub(crate) fn reason(refused: u32) -> String {
    let Some(number) = refused.checked_sub(1) else {
        return "syscall refused (not named: the app has no SIGSYS handler)".into();
    };
    match NAMES
        .iter()
        .find(|(n, _)| u32::try_from(*n).is_ok_and(|n| n == number))
    {
        Some((_, name)) => format!("syscall {name}"),
        None => format!("syscall {number}"),
    }
}

#[cfg(test)]
#[path = "syscalls_tests.rs"]
mod syscalls_tests;
