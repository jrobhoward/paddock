//! FreeBSD: `pdfork` gives the helper a process descriptor per instance
//! (passed to the node too), whose exit a kqueue reports with its status
//! (`EVFILT_PROCDESC`, `NOTE_EXIT`). With `JAIL`, the helper creates a
//! persistent jail per instance on its own nullfs view of the shared jail
//! root (devfs ruleset, the app's directory mounted read-write), adds `rctl`
//! rules for the jail, the child attaches, and the helper then makes the jail
//! non-persistent so it goes with its last process. Without a jail, `RCTL`
//! puts per-process rules on the child before it drops to the app's uid.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::helper::{self, Config, Proc};
use crate::os;
use crate::wire::{self, Spawn};

pub struct Instance {
    #[allow(dead_code)]
    id: u64,
    app: String,
    pid: i32,
    pd: OwnedFd,
    jid: Option<i32>,
    root: Option<PathBuf>,
}

impl Instance {
    pub fn pid(&self) -> i32 { self.pid }
    pub fn cgroup(&self) -> String { String::new() }
    pub fn dup_handle(&self) -> io::Result<OwnedFd> { self.pd.try_clone() }
}

pub fn check_flags(flags: u32) -> Result<(), String> {
    if flags & !(wire::JAIL | wire::RCTL) != 0 {
        return Err(format!("flags {flags:#x}: cgroups and namespaces are Linux's"));
    }
    Ok(())
}

pub fn init(c: &Config) -> io::Result<()> {
    if !c.jails.as_os_str().is_empty() {
        std::fs::create_dir_all(&c.jails)?;
    }
    Ok(())
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

fn pdfork() -> io::Result<(i32, Option<OwnedFd>)> {
    let mut pd: libc::c_int = -1;
    let pid = unsafe { libc::pdfork(&mut pd, libc::PD_CLOEXEC) };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((pid, (pid != 0).then(|| unsafe { OwnedFd::from_raw_fd(pd) })))
}

pub fn start_node(c: &Config, sock: RawFd, args: &[String]) -> io::Result<Proc> {
    let program = cstr(&c.node_program);
    let mut argv = vec![program.clone()];
    argv.extend(args.iter().map(|a| CString::new(a.as_str()).unwrap()));
    let (er, ew) = helper::pipe_cloexec()?;
    let (pid, pd) = pdfork()?;
    if pid == 0 {
        let ew = ew.as_raw_fd();
        if os::place_fds(&[sock]).is_err() { helper::child_fail(ew, 0) }
        if helper::drop_to(c.node_uid, c.node_gid).is_err() { helper::child_fail(ew, 1) }
        exec(&program, &argv);
        helper::child_fail(ew, 2)
    }
    drop(ew);
    helper::child_result(er, &["fds", "setuid", "exec"]).map_err(io::Error::other)?;
    Ok(Proc { pid, handle: pd.unwrap() })
}

pub fn reap_node(node: &Proc) -> String {
    let kq = Kq::new().unwrap();
    kq.add(node.handle.as_raw_fd(), 0);
    match kq.wait(-1).first() {
        Some(&(_, status)) => {
            let (code, sig) = helper::decode_status(status);
            if sig == 0 { format!("exit {code}") } else { format!("signal {sig}") }
        }
        None => "unknown".into(),
    }
}

/// SIGKILL to a jailed instance removes its jail, which kills every
/// process in it (what `cgroup.kill` does on Linux).
pub fn signal(inst: &Instance, sig: i32) -> io::Result<()> {
    if let (Some(j), libc::SIGKILL) = (inst.jid, sig) {
        if unsafe { libc::jail_remove(j) } == 0 {
            return Ok(());
        }
    }
    os::cvt(unsafe { libc::pdkill(inst.pd.as_raw_fd(), sig) }).map(drop)
}

fn nmount(params: &[(&str, &[u8])], flags: libc::c_int) -> io::Result<()> {
    let mut bufs: Vec<Vec<u8>> = Vec::new();
    for (k, v) in params {
        bufs.push(CString::new(*k).unwrap().into_bytes_with_nul());
        bufs.push(v.to_vec());
    }
    let mut errmsg = vec![0u8; 256];
    let mut iov: Vec<libc::iovec> = bufs.iter_mut().map(|b| libc::iovec { iov_base: b.as_mut_ptr().cast(), iov_len: b.len() }).collect();
    iov.push(libc::iovec { iov_base: c"errmsg".as_ptr() as *mut _, iov_len: 7 });
    iov.push(libc::iovec { iov_base: errmsg.as_mut_ptr().cast(), iov_len: errmsg.len() });
    if unsafe { libc::nmount(iov.as_mut_ptr(), iov.len() as u32, flags) } == -1 {
        let e = io::Error::last_os_error();
        let n = errmsg.iter().position(|&b| b == 0).unwrap_or(0);
        return Err(io::Error::new(e.kind(), format!("nmount: {e}: {}", String::from_utf8_lossy(&errmsg[..n]))));
    }
    Ok(())
}

fn pbytes(p: &Path) -> Vec<u8> {
    cstr(p).into_bytes_with_nul()
}

/// An instance's root: a read-only nullfs view of the shared jail root, a
/// devfs with the helper's ruleset, and the app's directory read-write at
/// its own path.
fn mount_root(c: &Config, s: &Spawn) -> io::Result<PathBuf> {
    let app_dir = c.apps.join(&s.app);
    let rel = app_dir.strip_prefix("/").unwrap();
    // The mount point must exist in the shared root.
    std::fs::create_dir_all(c.jail_root.join(rel))?;
    let dir = c.jails.join(format!("i{}", s.id));
    std::fs::create_dir_all(&dir)?;
    nmount(&[("fstype", b"nullfs\0"), ("fspath", &pbytes(&dir)), ("target", &pbytes(&c.jail_root))], libc::MNT_RDONLY)?;
    let rs = CString::new(c.devfs_ruleset.as_str()).unwrap().into_bytes_with_nul();
    nmount(&[("fstype", b"devfs\0"), ("fspath", &pbytes(&dir.join("dev"))), ("ruleset", &rs)], 0)?;
    nmount(&[("fstype", b"nullfs\0"), ("fspath", &pbytes(&dir.join(rel))), ("target", &pbytes(&app_dir))], 0)?;
    Ok(dir)
}

/// Roots whose unmount found them busy: a dying jail is freed on a
/// taskqueue (PB5), so for a moment after its last process it still holds
/// its root. Retried from the helper's loop; forced after 5 s.
static PENDING: std::sync::Mutex<Vec<(PathBuf, Option<String>, std::time::Instant)>> = std::sync::Mutex::new(Vec::new());

/// Unmount an instance root; false if it is still busy.
fn try_unmount(c: &Config, dir: &Path, app: Option<&str>, force: bool) -> bool {
    let mut paths = Vec::new();
    if let Some(app) = app {
        paths.push(dir.join(c.apps.join(app).strip_prefix("/").unwrap()));
    }
    paths.push(dir.join("dev"));
    paths.push(dir.to_path_buf());
    for p in paths {
        let cp = cstr(&p);
        let flags = if force { libc::MNT_FORCE } else { 0 };
        if unsafe { libc::unmount(cp.as_ptr(), flags) } != 0 && os::errno() != libc::EINVAL {
            return false;
        }
    }
    let _ = std::fs::remove_dir(dir);
    true
}

fn unmount_root(c: &Config, dir: &Path, app: Option<&str>) {
    if !try_unmount(c, dir, app, false) {
        PENDING.lock().unwrap().push((dir.to_path_buf(), app.map(String::from), std::time::Instant::now()));
    }
}

pub fn pending() -> bool {
    !PENDING.lock().unwrap().is_empty()
}

pub fn retry_pending(c: &Config) {
    PENDING.lock().unwrap().retain(|(dir, app, since)| {
        let force = since.elapsed() > std::time::Duration::from_secs(5);
        if force {
            eprintln!("helper: forcing the unmount of {}", dir.display());
        }
        !try_unmount(c, dir, app.as_deref(), force)
    });
}

struct JailParams(Vec<Vec<u8>>);

impl JailParams {
    fn set(&mut self, k: &str, v: Vec<u8>) -> &mut Self {
        self.0.push(CString::new(k).unwrap().into_bytes_with_nul());
        self.0.push(v);
        self
    }
    fn call(&mut self, flags: libc::c_int) -> io::Result<i32> {
        let mut errmsg = vec![0u8; 256];
        let mut iov: Vec<libc::iovec> = self.0.iter_mut().map(|b| libc::iovec { iov_base: b.as_mut_ptr().cast(), iov_len: b.len() }).collect();
        iov.push(libc::iovec { iov_base: c"errmsg".as_ptr() as *mut _, iov_len: 7 });
        iov.push(libc::iovec { iov_base: errmsg.as_mut_ptr().cast(), iov_len: errmsg.len() });
        let jid = unsafe { libc::jail_set(iov.as_mut_ptr(), iov.len() as u32, flags) };
        if jid < 0 {
            let e = io::Error::last_os_error();
            let n = errmsg.iter().position(|&b| b == 0).unwrap_or(0);
            return Err(io::Error::new(e.kind(), format!("jail_set: {e}: {}", String::from_utf8_lossy(&errmsg[..n]))));
        }
        Ok(jid)
    }
}

fn int(v: libc::c_int) -> Vec<u8> {
    v.to_ne_bytes().to_vec()
}

unsafe extern "C" {
    fn rctl_add_rule(inbufp: *const libc::c_char, inbuflen: libc::size_t, outbufp: *mut libc::c_char, outbuflen: libc::size_t) -> libc::c_int;
    fn rctl_remove_rule(inbufp: *const libc::c_char, inbuflen: libc::size_t, outbufp: *mut libc::c_char, outbuflen: libc::size_t) -> libc::c_int;
}

fn rctl_add(rule: &str) -> io::Result<()> {
    let r = CString::new(rule).unwrap();
    let b = r.as_bytes_with_nul();
    os::cvt(unsafe { rctl_add_rule(r.as_ptr(), b.len(), std::ptr::null_mut(), 0) }).map(drop)
}

fn rctl_remove(filter: &str) {
    let r = CString::new(filter).unwrap();
    let b = r.as_bytes_with_nul();
    unsafe { rctl_remove_rule(r.as_ptr(), b.len(), std::ptr::null_mut(), 0) };
}

pub fn spawn(c: &Config, s: &Spawn, program: &Path, fds: &[OwnedFd]) -> Result<Instance, String> {
    let rctl = s.flags & wire::RCTL != 0;
    let (root, jid) = if s.flags & wire::JAIL != 0 {
        let root = mount_root(c, s).map_err(|e| format!("jail root: {e}"))?;
        let jid = JailParams(Vec::new())
            .set("path", pbytes(&root))
            .set("name", CString::new(format!("s36-i{}", s.id)).unwrap().into_bytes_with_nul())
            .set("host.hostname", b"s36-app\0".to_vec())
            .set("ip4", int(libc::JAIL_SYS_DISABLE))
            .set("ip6", int(libc::JAIL_SYS_DISABLE))
            .set("persist", Vec::new())
            .call(libc::JAIL_CREATE);
        let jid = match jid {
            Ok(j) => j,
            Err(e) => {
                unmount_root(c, &root, Some(&s.app));
                return Err(format!("jail: {e}"));
            }
        };
        if rctl {
            for r in [format!("jail:{jid}:memoryuse:deny={}", s.memory_max), format!("jail:{jid}:maxproc:deny={}", s.pids_max)] {
                if let Err(e) = rctl_add(&r) {
                    return Err(format!("rctl {r}: {e}"));
                }
            }
        }
        (Some(root), Some(jid))
    } else {
        (None, None)
    };
    let prog = cstr(program);
    let mut argv = vec![prog.clone()];
    argv.extend(s.args.iter().map(|a| CString::new(a.as_str()).unwrap()));
    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let (er, ew) = helper::pipe_cloexec().map_err(|e| e.to_string())?;
    let (pid, pd) = pdfork().map_err(|e| format!("pdfork: {e}"))?;
    if pid == 0 {
        let ew = ew.as_raw_fd();
        if rctl && jid.is_none() {
            let me = unsafe { libc::getpid() };
            if rctl_add(&format!("process:{me}:memoryuse:deny={}", s.memory_max)).is_err() { helper::child_fail(ew, 0) }
        }
        if let Some(j) = jid {
            if unsafe { libc::jail_attach(j) } != 0 { helper::child_fail(ew, 1) }
        }
        if os::place_fds(&raw).is_err() { helper::child_fail(ew, 2) }
        if helper::drop_to(s.uid, s.uid).is_err() { helper::child_fail(ew, 3) }
        exec(&prog, &argv);
        helper::child_fail(ew, 4)
    }
    drop(ew);
    let pd = pd.unwrap();
    let result = helper::child_result(er, &["rctl", "jail_attach", "fds", "setuid", "exec"]);
    if let Some(j) = jid {
        // From now on the jail goes with its last process.
        let _ = JailParams(Vec::new()).set("jid", int(j)).set("nopersist", Vec::new()).call(libc::JAIL_UPDATE);
    }
    if let Err(e) = result {
        drop(pd);
        if let Some(r) = &root {
            unmount_root(c, r, Some(&s.app));
        }
        return Err(e);
    }
    Ok(Instance { id: s.id, app: s.app.clone(), pid, pd, jid, root })
}

pub fn finish(c: &Config, inst: Instance) {
    drop(inst.pd);
    if let Some(j) = inst.jid {
        // Anything the app forked dies with the jail.
        unsafe { libc::jail_remove(j) };
        rctl_remove(&format!("jail:{j}"));
    }
    if let Some(r) = &inst.root {
        unmount_root(c, r, Some(&inst.app));
    }
}

struct Kq(OwnedFd);

impl Kq {
    fn new() -> io::Result<Kq> {
        let fd = os::cvt(unsafe { libc::kqueue() })?;
        os::cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) })?;
        Ok(Kq(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
    fn add(&self, pd: RawFd, id: u64) {
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = pd as usize;
        ev.filter = libc::EVFILT_PROCDESC;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_EXIT;
        ev.udata = id as *mut libc::c_void;
        unsafe { libc::kevent(self.0.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    }
    /// (id, wait status) of each exit reported; `timeout_ms` < 0 waits.
    fn wait(&self, timeout_ms: i64) -> Vec<(u64, i32)> {
        let mut evs: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
        let ts = libc::timespec { tv_sec: (timeout_ms.max(0) / 1000) as _, tv_nsec: ((timeout_ms.max(0) % 1000) * 1_000_000) as _ };
        let tsp = if timeout_ms < 0 { std::ptr::null() } else { &ts as *const libc::timespec };
        let n = unsafe { libc::kevent(self.0.as_raw_fd(), std::ptr::null(), 0, evs.as_mut_ptr(), evs.len() as i32, tsp) };
        (0..n.max(0) as usize).map(|i| (evs[i].udata as u64, evs[i].data as i32)).collect()
    }
}

/// Waits on instances through one kqueue, which polls readable when one
/// exits.
pub struct Waiter(Kq);

impl Waiter {
    pub fn new() -> Waiter { Waiter(Kq::new().expect("kqueue")) }
    pub fn add(&mut self, id: u64, inst: &Instance) { self.0.add(inst.pd.as_raw_fd(), id) }
    pub fn fds(&self, _insts: &BTreeMap<u64, Instance>) -> Vec<RawFd> { vec![self.0.0.as_raw_fd()] }
    pub fn collect(&mut self, insts: &mut BTreeMap<u64, Instance>, rev: &[libc::c_short]) -> Vec<(u64, i32, i32, i32)> {
        if rev.first().copied().unwrap_or(0) == 0 {
            return Vec::new();
        }
        self.0
            .wait(0)
            .into_iter()
            .filter_map(|(id, status)| {
                let pid = insts.get(&id)?.pid;
                let (code, sig) = helper::decode_status(status);
                Some((id, pid, code, sig))
            })
            .collect()
    }
}
