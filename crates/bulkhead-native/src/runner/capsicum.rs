//! The runner's fence on FreeBSD (ROADMAP FB2): Capsicum.
//!
//! Before the app's code runs, the runner limits the rights of every
//! descriptor the app inherits to what its use needs, opens each preopened
//! directory or file as a descriptor of its own (named in
//! [`crate::preopens::FDS_ENV`]: capability mode refuses opening by path),
//! and enters capability mode. Capability mode forbids `execve`, so the
//! runner opens the run-time linker, the program and the library
//! directories first and `fexecve`s the linker in its direct-execution
//! mode (`ld-elf.so.1 -f FD`), which finds the libraries through
//! `LD_LIBRARY_PATH_FDS` (rtld(1)). The app never runs outside capability
//! mode, a hostile binary included; what it can reach is its descriptors,
//! with their rights.
//!
//! In capability mode a path open, `connect`, `bind` and signals to other
//! processes are refused with `ECAPMODE`; a socket may be made but neither
//! connected nor bound. Rights a descriptor lacks are refused with
//! `ENOTCAPABLE`. A cost on KVM virtual machines: libc cannot map
//! `/dev/pvclock` once in capability mode, so a clock read is a system
//! call (about 160 ns against 50 on the FreeBSD VM, 2026-11-12); bare
//! metal with a TSC timecounter needs no device.

use std::ffi::CString;
use std::os::fd::RawFd;
use std::process::ExitCode;

use super::Args;
use crate::channel::{BELL_FD, DOOR_FD, FDS_FD, MEMFD_FD};
use crate::freebsd::{limit_ioctls, limit_rights, rights};

/// The directories the run-time linker loads libraries from, where they
/// exist.
const LIBRARY_DIRS: [&str; 3] = ["/lib", "/usr/lib", "/usr/local/lib"];

/// The run-time linker.
const RTLD: &str = "/libexec/ld-elf.so.1";

/// Rights a read-only preopened directory gives what is opened under it.
const DIR_READ: &[u64] = &[
    libc::CAP_LOOKUP,
    libc::CAP_READ,
    libc::CAP_SEEK,
    libc::CAP_FSTAT,
    libc::CAP_FSTATAT,
    libc::CAP_FCNTL,
    libc::CAP_EVENT,
    libc::CAP_MMAP_R,
];

/// What a read-write preopened directory adds.
const DIR_WRITE: &[u64] = &[
    libc::CAP_CREATE,
    libc::CAP_WRITE,
    libc::CAP_FTRUNCATE,
    libc::CAP_FSYNC,
    libc::CAP_UNLINKAT,
    libc::CAP_MKDIRAT,
    libc::CAP_RENAMEAT_SOURCE,
    libc::CAP_RENAMEAT_TARGET,
    libc::CAP_MMAP_W,
];

/// Limit `fd` to `list`; a descriptor that is not open is left alone.
fn limit(fd: RawFd, list: &[u64]) -> Result<(), String> {
    match limit_rights(fd, list) {
        Err(e) if e.raw_os_error() != Some(libc::EBADF) => {
            Err(format!("limiting descriptor {fd}: {e}"))
        }
        _ => Ok(()),
    }
}

/// Open `path` for the runner's use, kept open across the exec.
fn open(path: &str, flags: libc::c_int) -> Result<RawFd, String> {
    let c = CString::new(path).map_err(|_| format!("{path}: a NUL in the path"))?;
    // SAFETY: a NUL-terminated path; the descriptor is the runner's.
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        return Err(format!("{path}: {}", std::io::Error::last_os_error()));
    }
    Ok(fd)
}

/// Limit what the app inherits from the node: stdio, the channel, the
/// descriptor socket, a driver's listening sockets.
fn limit_inherited(args: &Args) -> Result<(), String> {
    limit(0, &[libc::CAP_READ, libc::CAP_EVENT, libc::CAP_FSTAT])?;
    // Rust's std polls 0 to 2 at start and, without `EVENT`, opens
    // /dev/null in their place (PB5).
    for fd in [1, 2] {
        limit(fd, &[libc::CAP_WRITE, libc::CAP_EVENT, libc::CAP_FSTAT])?;
    }
    limit(MEMFD_FD, rights::MEMFD)?;
    limit(BELL_FD, rights::BELL)?;
    limit(DOOR_FD, rights::DOOR)?;
    limit(FDS_FD, rights::DOOR)?;
    let mut listener = rights::STREAM.to_vec();
    listener.push(libc::CAP_ACCEPT);
    for fd in &args.accept_fds {
        limit(*fd, &listener)?;
        limit_ioctls(*fd, rights::STREAM_IOCTLS)
            .map_err(|e| format!("limiting descriptor {fd}'s ioctls: {e}"))?;
    }
    Ok(())
}

/// Open each preopen (`BULKHEAD_PREOPENS`) as a descriptor with the rights
/// its mode gives, in order: the value of [`crate::preopens::FDS_ENV`].
fn open_preopens(args: &Args) -> Result<String, String> {
    let mut fds = Vec::new();
    for p in crate::preopens::from_env() {
        let host = p
            .host
            .to_str()
            .ok_or_else(|| format!("{}: not UTF-8", p.host.display()))?;
        let dir = p.host.is_dir();
        let flags = libc::O_RDONLY | if dir { libc::O_DIRECTORY } else { 0 };
        // One its user may not open is left out, as on Linux, where an open
        // under it fails and the app runs.
        let fd = match open(host, flags) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("bulkhead-runner: preopen {}: {e}", p.guest);
                fds.push("-".into());
                continue;
            }
        };
        let mut list = DIR_READ.to_vec();
        if dir && args.write.iter().any(|w| w == &p.host) {
            list.extend_from_slice(DIR_WRITE);
        }
        limit(fd, &list)?;
        fds.push(fd.to_string());
    }
    Ok(fds.join(","))
}

/// Fence this process and execute `program` in capability mode, with
/// `env` (the variables the app may know); returns only on failure.
#[must_use]
pub fn exec(args: &Args, program: &std::path::Path, env: Vec<(String, String)>) -> ExitCode {
    match prepare_and_exec(args, program, env) {
        Err(e) => {
            eprintln!("bulkhead-runner: capsicum: {e}");
            ExitCode::from(2)
        }
    }
}

fn prepare_and_exec(
    args: &Args,
    program: &std::path::Path,
    mut env: Vec<(String, String)>,
) -> Result<std::convert::Infallible, String> {
    limit_inherited(args)?;
    let preopens = open_preopens(args)?;
    if !preopens.is_empty() {
        env.push((crate::preopens::FDS_ENV.into(), preopens));
    }
    let rtld = open(RTLD, libc::O_RDONLY | libc::O_CLOEXEC)?;
    limit(
        rtld,
        &[
            libc::CAP_FEXECVE,
            libc::CAP_READ,
            libc::CAP_MMAP_RX,
            libc::CAP_FSTAT,
        ],
    )?;
    let path = program
        .to_str()
        .ok_or_else(|| format!("{}: not UTF-8", program.display()))?;
    let prog = open(path, libc::O_RDONLY)?;
    limit(
        prog,
        &[
            libc::CAP_READ,
            libc::CAP_SEEK,
            libc::CAP_MMAP_RX,
            libc::CAP_FSTAT,
        ],
    )?;
    let mut libs = Vec::new();
    for dir in LIBRARY_DIRS {
        if std::path::Path::new(dir).is_dir() {
            let fd = open(dir, libc::O_RDONLY | libc::O_DIRECTORY)?;
            limit(
                fd,
                &[
                    libc::CAP_LOOKUP,
                    libc::CAP_READ,
                    libc::CAP_SEEK,
                    libc::CAP_MMAP_RX,
                    libc::CAP_FSTAT,
                    libc::CAP_FCNTL,
                ],
            )?;
            libs.push(fd.to_string());
        }
    }
    env.push(("LD_LIBRARY_PATH_FDS".into(), libs.join(":")));
    // SAFETY: no arguments; from here this process and what it executes
    // reach only their descriptors.
    if unsafe { libc::cap_enter() } != 0 {
        return Err(format!("cap_enter: {}", std::io::Error::last_os_error()));
    }
    let name = program
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("app")
        .to_string();
    let argv: Vec<CString> = [
        "ld-elf.so.1".to_string(),
        "-f".into(),
        prog.to_string(),
        "--".into(),
        name,
    ]
    .into_iter()
    .map(|a| CString::new(a).map_err(|_| "a NUL in an argument".to_string()))
    .collect::<Result<_, _>>()?;
    let envs: Vec<CString> = env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")).map_err(|_| format!("a NUL in {k}")))
        .collect::<Result<_, _>>()?;
    let mut argp: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argp.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = envs.iter().map(|e| e.as_ptr()).collect();
    envp.push(std::ptr::null());
    // SAFETY: NULL-terminated arrays of NUL-terminated strings that live
    // until the call, which returns only on failure.
    unsafe { libc::fexecve(rtld, argp.as_ptr(), envp.as_ptr()) };
    Err(format!(
        "fexecve {RTLD}: {}",
        std::io::Error::last_os_error()
    ))
}
