//! The helper: started as root, it starts the node under the node's user
//! with a seqpacket socket on fd 3, then serves the node's requests on it
//! and nothing else. It is synchronous and single-threaded: one poll over the
//! node's socket, the node's process handle, and the instances' handles.
//!
//! Its checks come from its own config, never from the node: the uid range,
//! the programs directory, the ports, the limits' ceilings.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use crate::os;
use crate::platform;
use crate::wire::{self, Rep, Req, Spawn};

#[derive(Debug, Clone)]
#[allow(dead_code)] // each system reads its own fields
pub struct Config {
    pub node_uid: u32,
    pub node_gid: u32,
    pub uids: (u32, u32),
    pub programs: PathBuf,
    pub node_program: PathBuf,
    pub ports: Vec<u16>,
    pub memory_max: u64,
    pub pids_max: u32,
    /// Where app directories live (`<apps>/<app>`, owned by the app's uid,
    /// 0700); made by the helper on an app's first spawn.
    pub apps: PathBuf,
    /// Linux: the cgroup the helper makes per-app and per-instance cgroups
    /// under.
    pub cgroup: PathBuf,
    /// Linux: the controllers enabled below `cgroup` (`+memory +pids +cpu`).
    pub cgroup_controllers: String,
    /// Linux: where minimal roots are built (in each instance's own mount
    /// namespace).
    pub roots: PathBuf,
    /// FreeBSD: the shared, read-only jail root each instance's root mirrors.
    pub jail_root: PathBuf,
    /// FreeBSD: per-instance jail roots are mounted here.
    pub jails: PathBuf,
    pub devfs_ruleset: String,
}

impl Config {
    fn load(path: &str) -> Config {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| fail(&format!("{path}: {e}")));
        let mut kv = HashMap::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap().trim();
            if line.is_empty() {
                continue;
            }
            let (k, v) = line.split_once('=').unwrap_or_else(|| fail(&format!("{path}: bad line {line:?}")));
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
        let get = |k: &str| kv.get(k).cloned().unwrap_or_else(|| fail(&format!("{path}: missing {k}")));
        let num = |k: &str| get(k).parse::<u64>().unwrap_or_else(|_| fail(&format!("{path}: {k} is not a number")));
        let (lo, hi) = get("uids").split_once('-').map(|(a, b)| (a.trim().parse().unwrap(), b.trim().parse().unwrap())).unwrap();
        Config {
            node_uid: num("node_uid") as u32,
            node_gid: num("node_gid") as u32,
            uids: (lo, hi),
            programs: get("programs").into(),
            node_program: get("node_program").into(),
            ports: get("ports").split(',').filter(|s| !s.trim().is_empty()).map(|p| p.trim().parse().unwrap()).collect(),
            memory_max: num("memory_max"),
            pids_max: num("pids_max") as u32,
            apps: get("apps").into(),
            cgroup: kv.get("cgroup").cloned().unwrap_or_default().into(),
            cgroup_controllers: kv.get("cgroup_controllers").cloned().unwrap_or_else(|| "+memory +pids +cpu".into()),
            roots: kv.get("roots").cloned().unwrap_or_default().into(),
            jail_root: kv.get("jail_root").cloned().unwrap_or_default().into(),
            jails: kv.get("jails").cloned().unwrap_or_default().into(),
            devfs_ruleset: kv.get("devfs_ruleset").cloned().unwrap_or_default(),
        }
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("helper: {msg}");
    std::process::exit(1)
}

/// A running process the helper started, and how to wait on it.
pub struct Proc {
    pub pid: i32,
    /// Linux: a pidfd. FreeBSD: a process descriptor.
    pub handle: OwnedFd,
}

pub fn main(args: &[String]) {
    let Some(conf_path) = args.first() else { fail("usage: s36 helper <config> -- node [options]") };
    let node_args: Vec<String> = match args.iter().position(|a| a == "--") {
        Some(i) => args[i + 1..].to_vec(),
        None => vec!["node".into()],
    };
    if unsafe { libc::geteuid() } != 0 {
        fail("must run as root");
    }
    let conf = Config::load(conf_path);
    platform::init(&conf).unwrap_or_else(|e| fail(&format!("init: {e}")));
    let (mine, theirs) = os::seqpacket_pair();
    let node = platform::start_node(&conf, theirs.as_raw_fd(), &node_args)
        .unwrap_or_else(|e| fail(&format!("starting the node: {e}")));
    drop(theirs);
    eprintln!("helper: node started as pid {} (uid {})", node.pid, conf.node_uid);
    let status = Helper { conf, sock: mine, insts: BTreeMap::new(), app_of_uid: HashMap::new(), waiter: platform::Waiter::new() }
        .run(&node);
    eprintln!("helper: node exited ({status}); helper exits");
}

struct Helper {
    conf: Config,
    sock: OwnedFd,
    insts: BTreeMap<u64, platform::Instance>,
    /// Which app each uid was first used for: a uid never serves two apps.
    app_of_uid: HashMap<u32, String>,
    waiter: platform::Waiter,
}

impl Helper {
    fn run(mut self, node: &Proc) -> String {
        let mut buf = vec![0u8; wire::MAX_MSG];
        let mut node_open = true;
        loop {
            let mut fds: Vec<RawFd> = vec![node.handle.as_raw_fd()];
            if node_open {
                fds.push(self.sock.as_raw_fd());
            }
            let watch = self.waiter.fds(&self.insts);
            let base = fds.len();
            fds.extend(&watch);
            // Cleanup the kernel has not let go of yet (a dying jail's
            // root) is retried every 20 ms until it goes.
            let timeout = if platform::pending() { 20 } else { -1 };
            let rev = os::poll(&fds, timeout).unwrap_or_else(|e| fail(&format!("poll: {e}")));
            platform::retry_pending(&self.conf);
            // Exits first, so an Exited event precedes the answer to a
            // request that came after it; the notice before the cleanup.
            for (id, pid, code, signal) in self.waiter.collect(&mut self.insts, &rev[base..]) {
                if node_open {
                    let _ = os::send(self.sock.as_raw_fd(), &Rep::Exited { id, pid, code, signal }.encode(), &[]);
                }
                if let Some(inst) = self.insts.remove(&id) {
                    platform::finish(&self.conf, inst);
                }
            }
            if node_open && rev[1] != 0 {
                match os::recv(self.sock.as_raw_fd(), &mut buf) {
                    Ok(Some((n, fds))) => {
                        let (rep, give) = match Req::decode(&buf[..n]) {
                            Some(req) => self.serve(req, fds),
                            None => (Rep::Refused { msg: "malformed request".into() }, None),
                        };
                        let give: Vec<RawFd> = give.iter().map(|f| f.as_raw_fd()).collect();
                        if os::send(self.sock.as_raw_fd(), &rep.encode(), &give).is_err() {
                            node_open = false;
                        }
                    }
                    Ok(None) | Err(_) => node_open = false,
                }
                if !node_open {
                    eprintln!("helper: node's socket closed; killing {} instances", self.insts.len());
                    self.kill_all();
                }
            }
            if rev[0] != 0 {
                if node_open {
                    eprintln!("helper: node exited; killing {} instances", self.insts.len());
                }
                self.kill_all();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while platform::pending() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    platform::retry_pending(&self.conf);
                }
                return platform::reap_node(node);
            }
        }
    }

    fn kill_all(&mut self) {
        for inst in self.insts.values() {
            let _ = platform::signal(inst, libc::SIGKILL);
        }
        // Wait for each, so cgroups and jail roots can go.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !self.insts.is_empty() && std::time::Instant::now() < deadline {
            let watch = self.waiter.fds(&self.insts);
            let rev = os::poll(&watch, 100).unwrap_or_default();
            for (id, ..) in self.waiter.collect(&mut self.insts, &rev) {
                if let Some(inst) = self.insts.remove(&id) {
                    platform::finish(&self.conf, inst);
                }
            }
        }
    }

    fn serve(&mut self, req: Req, fds: Vec<OwnedFd>) -> (Rep, Option<OwnedFd>) {
        match req {
            Req::Spawn(s) => match self.spawn(&s, fds) {
                Ok((rep, handle)) => (rep, Some(handle)),
                Err(msg) => (Rep::Refused { msg }, None),
            },
            Req::Signal { id, sig } => match self.insts.get(&id) {
                None => (Rep::Refused { msg: format!("no instance {id}") }, None),
                Some(_) if !matches!(sig, libc::SIGTERM | libc::SIGKILL | libc::SIGCONT | 0) => {
                    (Rep::Refused { msg: format!("signal {sig} is not one the node may send") }, None)
                }
                Some(inst) => match platform::signal(inst, sig) {
                    Ok(()) => (Rep::Done, None),
                    Err(e) => (Rep::Refused { msg: format!("signal: {e}") }, None),
                },
            },
            Req::Bind { port } => {
                if !self.conf.ports.contains(&port) {
                    return (Rep::Refused { msg: format!("port {port} is not in the helper's ports") }, None);
                }
                match bind_tcp(port) {
                    Ok(fd) => (Rep::Bound { port }, Some(fd)),
                    Err(e) => (Rep::Refused { msg: format!("bind {port}: {e}") }, None),
                }
            }
        }
    }

    fn spawn(&mut self, s: &Spawn, fds: Vec<OwnedFd>) -> Result<(Rep, OwnedFd), String> {
        let c = &self.conf;
        if s.app.is_empty() || s.app.len() > 32 || !s.app.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Err(format!("app name {:?} is not [a-z0-9-]{{1,32}}", s.app));
        }
        if s.uid < c.uids.0 || s.uid > c.uids.1 || s.uid == c.node_uid || s.uid == 0 {
            return Err(format!("uid {} is outside {}-{}", s.uid, c.uids.0, c.uids.1));
        }
        match self.app_of_uid.get(&s.uid) {
            Some(app) if *app != s.app => return Err(format!("uid {} belongs to app {app}", s.uid)),
            _ => {}
        }
        if self.app_of_uid.iter().any(|(u, a)| *a == s.app && *u != s.uid) {
            return Err(format!("app {} already runs under another uid", s.app));
        }
        let program = Path::new(&s.program).canonicalize().map_err(|e| format!("program {}: {e}", s.program))?;
        if !program.starts_with(&c.programs) {
            return Err(format!("program {} is outside {}", program.display(), c.programs.display()));
        }
        if s.memory_max == 0 || s.memory_max > c.memory_max || s.pids_max == 0 || s.pids_max > c.pids_max {
            return Err("limits above the helper's ceilings".into());
        }
        if fds.len() > wire::MAX_FDS {
            return Err("too many descriptors".into());
        }
        if self.insts.contains_key(&s.id) {
            return Err(format!("instance {} exists", s.id));
        }
        platform::check_flags(s.flags)?;
        app_dir(c, &s.app, s.uid).map_err(|e| format!("app directory: {e}"))?;
        let inst = platform::spawn(c, s, &program, &fds)?;
        self.app_of_uid.insert(s.uid, s.app.clone());
        let rep = Rep::Spawned { id: s.id, pid: inst.pid(), cgroup: inst.cgroup() };
        let handle = inst.dup_handle().map_err(|e| format!("dup handle: {e}"))?;
        self.waiter.add(s.id, &inst);
        self.insts.insert(s.id, inst);
        Ok((rep, handle))
    }
}

/// `<apps>/<app>`, owned by the app's uid and group, mode 0700.
fn app_dir(c: &Config, app: &str, uid: u32) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = c.apps.join(app);
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    std::os::unix::fs::chown(&dir, Some(uid), Some(uid))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn bind_tcp(port: u16) -> io::Result<OwnedFd> {
    let l = std::net::TcpListener::bind(("0.0.0.0", port))?;
    Ok(OwnedFd::from(l))
}

/// In a child after fork, before exec: drop to `uid`/`gid` with no
/// supplementary groups. Only system calls.
pub fn drop_to(uid: u32, gid: u32) -> io::Result<()> {
    os::cvt(unsafe { libc::setgroups(0, std::ptr::null()) })?;
    os::cvt(unsafe { libc::setresgid(gid, gid, gid) })?;
    os::cvt(unsafe { libc::setresuid(uid, uid, uid) })?;
    Ok(())
}

/// Report a failure from a child after fork through the error pipe, then
/// exit: `step` names what failed.
pub fn child_fail(err_pipe: RawFd, step: u8) -> ! {
    let e = os::errno();
    let mut b = [0u8; 5];
    b[0] = step;
    b[1..].copy_from_slice(&e.to_le_bytes());
    unsafe {
        libc::write(err_pipe, b.as_ptr().cast(), b.len());
        libc::_exit(127)
    }
}

/// Read the error pipe after fork: `Ok` once exec closed it, or the step
/// and errno the child reported.
pub fn child_result(read_end: OwnedFd, steps: &[&str]) -> Result<(), String> {
    let mut b = [0u8; 5];
    let n = loop {
        let n = unsafe { libc::read(read_end.as_raw_fd(), b.as_mut_ptr().cast(), b.len()) };
        if n < 0 && os::errno() == libc::EINTR {
            continue;
        }
        break n;
    };
    if n <= 0 {
        return Ok(());
    }
    let step = steps.get(b[0] as usize).copied().unwrap_or("?");
    let e = i32::from_le_bytes(b[1..].try_into().unwrap());
    Err(format!("{step}: {}", os::errname(e)))
}

pub fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut p = [0; 2];
    os::cvt(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) })?;
    Ok(unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) })
}

/// An exit status as (code, signal).
pub fn decode_status(status: i32) -> (i32, i32) {
    if libc::WIFEXITED(status) {
        (libc::WEXITSTATUS(status), 0)
    } else if libc::WIFSIGNALED(status) {
        (-1, libc::WTERMSIG(status))
    } else {
        (-1, -1)
    }
}
