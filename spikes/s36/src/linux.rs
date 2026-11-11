//! Linux: `clone3` with a pidfd, straight into the instance's cgroup
//! (`CLONE_INTO_CGROUP`), optionally in new mount, PID, network, IPC and UTS
//! namespaces (no user namespace: the helper is root), optionally with a
//! minimal root (`pivot_root` onto a tmpfs holding read-only binds of the
//! system's library directories, the programs directory and the app's own
//! directory).

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::helper::{self, Config, Proc};
use crate::os;
use crate::wire::{self, Spawn};

const CLONE_PIDFD: u64 = 0x1000;
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;
const P_PIDFD: libc::idtype_t = 3;

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

pub struct Instance {
    id: u64,
    pid: i32,
    pidfd: OwnedFd,
    cgroup: Option<PathBuf>,
}

impl Instance {
    pub fn pid(&self) -> i32 { self.pid }
    pub fn cgroup(&self) -> String { self.cgroup.as_ref().map(|p| p.display().to_string()).unwrap_or_default() }
    pub fn dup_handle(&self) -> io::Result<OwnedFd> { self.pidfd.try_clone() }
}

pub fn check_flags(flags: u32) -> Result<(), String> {
    if flags & !(wire::CGROUP | wire::NS | wire::ROOT) != 0 {
        return Err(format!("flags {flags:#x}: jails and rctl are FreeBSD's"));
    }
    if flags & wire::ROOT != 0 && flags & wire::NS == 0 {
        return Err("a minimal root needs namespaces".into());
    }
    Ok(())
}

pub fn init(c: &Config) -> io::Result<()> {
    if !c.cgroup.as_os_str().is_empty() {
        std::fs::create_dir_all(&c.cgroup)?;
        std::fs::write(c.cgroup.join("cgroup.subtree_control"), &c.cgroup_controllers)?;
    }
    if !c.roots.as_os_str().is_empty() {
        std::fs::create_dir_all(&c.roots)?;
    }
    Ok(())
}

fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) }) }
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

fn exec(program: &CString, argv: &[CString]) {
    let mut a: Vec<*const libc::c_char> = argv.iter().map(|s| s.as_ptr()).collect();
    a.push(std::ptr::null());
    let env: [*const libc::c_char; 1] = [std::ptr::null()];
    unsafe { libc::execve(program.as_ptr(), a.as_ptr(), env.as_ptr()) };
}

pub fn start_node(c: &Config, sock: RawFd, args: &[String]) -> io::Result<Proc> {
    let program = cstr(&c.node_program);
    let mut argv = vec![program.clone()];
    argv.extend(args.iter().map(|a| CString::new(a.as_str()).unwrap()));
    let (er, ew) = helper::pipe_cloexec()?;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        let ew = ew.as_raw_fd();
        if os::place_fds(&[sock]).is_err() { helper::child_fail(ew, 0) }
        if helper::drop_to(c.node_uid, c.node_gid).is_err() { helper::child_fail(ew, 1) }
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 { helper::child_fail(ew, 2) }
        exec(&program, &argv);
        helper::child_fail(ew, 3)
    }
    drop(ew);
    helper::child_result(er, &["fds", "setuid", "pdeathsig", "exec"]).map_err(io::Error::other)?;
    Ok(Proc { pid, handle: pidfd_open(pid)? })
}

pub fn reap_node(node: &Proc) -> String {
    match wait_pidfd(node.handle.as_raw_fd(), 0) {
        Some((code, 0)) => format!("exit {code}"),
        Some((_, sig)) => format!("signal {sig}"),
        None => "unknown".into(),
    }
}

/// `waitid(P_PIDFD)`: (code, signal) once it has exited.
fn wait_pidfd(fd: RawFd, flags: libc::c_int) -> Option<(i32, i32)> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::waitid(P_PIDFD, fd as libc::id_t, &mut info, libc::WEXITED | flags) };
    if r != 0 || unsafe { info.si_pid() } == 0 {
        return None;
    }
    let status = unsafe { info.si_status() };
    Some(if info.si_code == libc::CLD_EXITED { (status, 0) } else { (-1, status) })
}

pub fn signal(inst: &Instance, sig: i32) -> io::Result<()> {
    pidfd_signal(inst.pidfd.as_raw_fd(), sig)
}

pub fn pidfd_signal(fd: RawFd, sig: i32) -> io::Result<()> {
    let r = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd, sig, std::ptr::null::<libc::siginfo_t>(), 0) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn make_cgroup(c: &Config, s: &Spawn) -> io::Result<PathBuf> {
    let app = c.cgroup.join(&s.app);
    if !app.exists() {
        std::fs::create_dir(&app)?;
        std::fs::write(app.join("cgroup.subtree_control"), &c.cgroup_controllers)?;
    }
    let leaf = app.join(format!("i{}", s.id));
    std::fs::create_dir(&leaf)?;
    std::fs::write(leaf.join("memory.max"), s.memory_max.to_string())?;
    std::fs::write(leaf.join("pids.max"), s.pids_max.to_string())?;
    // The node may end the instance, and everything it forked, without a
    // capability or a request: write access to this one file.
    std::os::unix::fs::chown(leaf.join("cgroup.kill"), Some(c.node_uid), None)?;
    Ok(leaf)
}

pub fn spawn(c: &Config, s: &Spawn, program: &Path, fds: &[OwnedFd]) -> Result<Instance, String> {
    let cgroup = if s.flags & wire::CGROUP != 0 { Some(make_cgroup(c, s).map_err(|e| format!("cgroup: {e}"))?) } else { None };
    let cgfd = match &cgroup {
        Some(p) => Some(std::fs::File::open(p).map_err(|e| format!("cgroup open: {e}"))?),
        None => None,
    };
    let ns = s.flags & wire::NS != 0;
    let root = (s.flags & wire::ROOT != 0).then(|| c.roots.join(format!("i{}", s.id)));
    if let Some(r) = &root {
        std::fs::create_dir_all(r).map_err(|e| format!("root dir: {e}"))?;
    }
    let app_dir = c.apps.join(&s.app);
    let prog = cstr(program);
    let mut argv = vec![prog.clone()];
    argv.extend(s.args.iter().map(|a| CString::new(a.as_str()).unwrap()));
    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let (er, ew) = helper::pipe_cloexec().map_err(|e| e.to_string())?;

    let mut flags = CLONE_PIDFD;
    if cgfd.is_some() {
        flags |= CLONE_INTO_CGROUP;
    }
    if ns {
        flags |= (libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWNET | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS) as u64;
    }
    let mut pidfd: RawFd = -1;
    let mut args = CloneArgs {
        flags,
        pidfd: &mut pidfd as *mut RawFd as u64,
        exit_signal: libc::SIGCHLD as u64,
        cgroup: cgfd.as_ref().map_or(0, |f| f.as_raw_fd() as u64),
        ..CloneArgs::default()
    };
    let pid = unsafe { libc::syscall(libc::SYS_clone3, &mut args as *mut CloneArgs, std::mem::size_of::<CloneArgs>()) } as i32;
    if pid < 0 {
        let e = io::Error::last_os_error();
        if let Some(p) = &cgroup { let _ = std::fs::remove_dir(p); }
        return Err(format!("clone3: {e}"));
    }
    if pid == 0 {
        // The child: the helper is single-threaded, so allocating here is safe.
        let ew = ew.as_raw_fd();
        if ns {
            if mount(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None).is_err() { helper::child_fail(ew, 0) }
            if unsafe { libc::sethostname(c"s36-app".as_ptr(), 7) } != 0 { helper::child_fail(ew, 1) }
            match &root {
                Some(r) => if build_root(r, &c.programs, &app_dir).is_err() { helper::child_fail(ew, 3) },
                None => if mount(Some("proc"), "/proc", Some("proc"), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None).is_err() { helper::child_fail(ew, 2) },
            }
        }
        if os::place_fds(&raw).is_err() { helper::child_fail(ew, 4) }
        if helper::drop_to(s.uid, s.uid).is_err() { helper::child_fail(ew, 5) }
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 { helper::child_fail(ew, 6) }
        exec(&prog, &argv);
        helper::child_fail(ew, 7)
    }
    drop(ew);
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
    if let Err(e) = helper::child_result(er, &["private mounts", "hostname", "proc", "minimal root", "fds", "setuid", "pdeathsig", "exec"]) {
        let _ = wait_pidfd(pidfd.as_raw_fd(), 0);
        if let Some(p) = &cgroup { let _ = std::fs::remove_dir(p); }
        if let Some(r) = &root { let _ = std::fs::remove_dir(r); }
        return Err(e);
    }
    Ok(Instance { id: s.id, pid, pidfd, cgroup })
}

fn mount(src: Option<&str>, dst: &str, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> io::Result<()> {
    let c = |s: Option<&str>| s.map(|s| CString::new(s).unwrap());
    let (src, dst, fstype, data) = (c(src), CString::new(dst).unwrap(), c(fstype), c(data));
    let p = |s: &Option<CString>| s.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
    os::cvt(unsafe { libc::mount(p(&src), dst.as_ptr(), p(&fstype), flags, p(&data).cast()) }).map(drop)
}

fn bind(src: &Path, dst: &Path, ro: bool) -> io::Result<()> {
    let (s, d) = (src.to_str().unwrap(), dst.to_str().unwrap());
    mount(Some(s), d, None, libc::MS_BIND | libc::MS_REC, None)?;
    let mut f = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_NOSUID | libc::MS_NODEV;
    if ro {
        f |= libc::MS_RDONLY;
    }
    mount(None, d, None, f, None)
}

/// In the child's own mount namespace: a tmpfs root holding only what an
/// app needs, read-only but for its own directory, then `pivot_root`.
fn build_root(root: &Path, programs: &Path, app_dir: &Path) -> io::Result<()> {
    let r = root.to_str().unwrap();
    mount(Some("tmpfs"), r, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some("mode=0755,size=1m"))?;
    let inside = |p: &Path| root.join(p.strip_prefix("/").unwrap());
    std::fs::create_dir(root.join("usr"))?;
    bind(Path::new("/usr"), &root.join("usr"), true)?;
    for l in ["bin", "sbin", "lib", "lib64", "lib32", "libx32"] {
        let host = Path::new("/").join(l);
        match std::fs::symlink_metadata(&host) {
            Ok(m) if m.file_type().is_symlink() => std::os::unix::fs::symlink(std::fs::read_link(&host)?, root.join(l))?,
            Ok(m) if m.is_dir() => {
                std::fs::create_dir(root.join(l))?;
                bind(&host, &root.join(l), true)?;
            }
            _ => {}
        }
    }
    std::fs::create_dir(root.join("etc"))?;
    if Path::new("/etc/ld.so.cache").exists() {
        std::fs::write(root.join("etc/ld.so.cache"), b"")?;
        bind(Path::new("/etc/ld.so.cache"), &root.join("etc/ld.so.cache"), true)?;
    }
    std::fs::create_dir_all(inside(programs))?;
    bind(programs, &inside(programs), true)?;
    std::fs::create_dir_all(inside(app_dir))?;
    bind(app_dir, &inside(app_dir), false)?;
    std::fs::create_dir(root.join("dev"))?;
    for n in ["null", "zero", "urandom"] {
        std::fs::write(root.join("dev").join(n), b"")?;
        bind(&Path::new("/dev").join(n), &root.join("dev").join(n), false)?;
    }
    std::fs::create_dir(root.join("tmp"))?;
    mount(Some("tmpfs"), root.join("tmp").to_str().unwrap(), Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some("mode=1777,size=1m"))?;
    std::fs::create_dir(root.join("proc"))?;
    mount(Some("proc"), root.join("proc").to_str().unwrap(), Some("proc"), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None)?;
    std::env::set_current_dir(root)?;
    let dot = c".";
    if unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    os::cvt(unsafe { libc::umount2(dot.as_ptr(), libc::MNT_DETACH) })?;
    std::env::set_current_dir("/")?;
    mount(None, "/", None, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV, None)
}

pub fn finish(c: &Config, inst: Instance) {
    if let Some(p) = &inst.cgroup {
        for _ in 0..50 {
            match std::fs::remove_dir(p) {
                Ok(()) => break,
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => std::thread::sleep(std::time::Duration::from_millis(2)),
                Err(e) => {
                    eprintln!("helper: rmdir {}: {e}", p.display());
                    break;
                }
            }
        }
    }
    let r = c.roots.join(format!("i{}", inst.id));
    let _ = std::fs::remove_dir(&r);
}

pub fn pending() -> bool { false }
pub fn retry_pending(_c: &Config) {}

/// Waits on instances through their pidfds, which poll readable on exit.
pub struct Waiter;

impl Waiter {
    pub fn new() -> Waiter { Waiter }
    pub fn add(&mut self, _id: u64, _inst: &Instance) {}
    pub fn fds(&self, insts: &std::collections::BTreeMap<u64, Instance>) -> Vec<RawFd> {
        insts.values().map(|i| i.pidfd.as_raw_fd()).collect()
    }
    pub fn collect(&mut self, insts: &mut std::collections::BTreeMap<u64, Instance>, rev: &[libc::c_short]) -> Vec<(u64, i32, i32, i32)> {
        let mut out = Vec::new();
        for (inst, &r) in insts.values().zip(rev) {
            if r != 0 {
                if let Some((code, sig)) = wait_pidfd(inst.pidfd.as_raw_fd(), libc::WNOHANG) {
                    out.push((inst.id, inst.pid, code, sig));
                }
            }
        }
        out
    }
}
